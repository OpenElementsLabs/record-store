//! S3-compatible HTTP protocol adapter.
//!
//! Supported operations are deliberately limited to bucket lifecycle,
//! single-part object lifecycle, range GET, and ListObjectsV2.

use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    net::SocketAddr,
    sync::Arc,
    time::Instant,
};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{ConnectInfo, Extension, Path, RawQuery, Request, State},
    http::{
        HeaderMap, HeaderValue, Method, StatusCode, Uri,
        header::{self, HeaderName},
    },
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, put},
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use futures_util::TryStreamExt;
use hmac::{Hmac, Mac};
use oes_audit::{AuditEvent, AuditRepository, AuditResult};
use oes_auth::{
    Action, AuthorizationContext, Authorizer, CredentialLookupError, Permission, Principal,
    SigningCredentialProvider, SigningSecret,
};
use oes_core::{
    BucketName, ByteRange, Checksum, CompletedPart, ETag, ObjectKey, ObjectMetadata,
    ObjectVersionRecord, PartNumber, UploadId, VersionId, VersioningState,
};
use oes_service::{
    CopyMetadataDirective, ServiceCompleteMultipartRequest, ServiceCopyRequest,
    ServiceCreateMultipartRequest, ServiceError, ServiceGetResult,
    ServiceListMultipartUploadsRequest, ServiceListRequest, ServiceListVersionsRequest,
    ServicePutRequest, ServiceUploadPartRequest, Services,
};
use oes_storage::upload_stream;
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tracing::info;
use uuid::Uuid;

/// Stable support level for the machine-testable S3 compatibility registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityStatus {
    /// The operation is implemented and covered by protocol tests.
    Implemented,
    /// A useful subset is implemented with explicit unsupported semantics.
    Partial,
    /// Requests are rejected with `NotImplemented`.
    Unsupported,
}

/// One low-cardinality S3 capability descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct S3Capability {
    pub name: &'static str,
    pub status: CapabilityStatus,
}

/// Testable compatibility surface. Keep this synchronized with routing and
/// protocol tests instead of maintaining a separate status document.
pub const S3_CAPABILITIES: &[S3Capability] = &[
    S3Capability {
        name: "SigV4HeaderAuthentication",
        status: CapabilityStatus::Implemented,
    },
    S3Capability {
        name: "PresignedGetObject",
        status: CapabilityStatus::Implemented,
    },
    S3Capability {
        name: "PresignedPutObject",
        status: CapabilityStatus::Implemented,
    },
    S3Capability {
        name: "BucketOperations",
        status: CapabilityStatus::Implemented,
    },
    S3Capability {
        name: "ObjectOperations",
        status: CapabilityStatus::Implemented,
    },
    S3Capability {
        name: "ListObjectsV2",
        status: CapabilityStatus::Implemented,
    },
    S3Capability {
        name: "MultipartUpload",
        status: CapabilityStatus::Implemented,
    },
    S3Capability {
        name: "UploadPartCopy",
        status: CapabilityStatus::Unsupported,
    },
    S3Capability {
        name: "ObjectVersioning",
        status: CapabilityStatus::Implemented,
    },
    S3Capability {
        name: "CopyObject",
        status: CapabilityStatus::Implemented,
    },
    S3Capability {
        name: "RangeAndConditionalReads",
        status: CapabilityStatus::Implemented,
    },
    S3Capability {
        name: "ClientSha256Checksums",
        status: CapabilityStatus::Implemented,
    },
    S3Capability {
        name: "ServerSideEncryptionHeaders",
        status: CapabilityStatus::Unsupported,
    },
    S3Capability {
        name: "AccessControlLists",
        status: CapabilityStatus::Unsupported,
    },
    S3Capability {
        name: "ObjectLock",
        status: CapabilityStatus::Unsupported,
    },
    S3Capability {
        name: "AwsChunkedEncoding",
        status: CapabilityStatus::Unsupported,
    },
];

const XML_CONTENT_TYPE: &str = "application/xml";
/// SHA-256 digest of an empty S3 request payload.
pub const EMPTY_PAYLOAD_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-amz-request-id");

/// Dependencies and security settings for the S3 protocol surface.
#[derive(Clone)]
pub struct S3State {
    services: Services,
    credentials: Arc<dyn SigningCredentialProvider>,
    authorizer: Option<Arc<dyn Authorizer>>,
    audit: Option<Arc<dyn AuditRepository>>,
    root_s3_enabled: bool,
    allowed_clock_skew: Duration,
    maximum_presign_seconds: i64,
    maximum_header_bytes: usize,
}

impl S3State {
    /// Constructs S3 state with a 15-minute signed-request skew allowance.
    #[must_use]
    pub fn new(services: Services, credentials: Arc<dyn SigningCredentialProvider>) -> Self {
        Self {
            services,
            credentials,
            authorizer: None,
            audit: None,
            root_s3_enabled: true,
            allowed_clock_skew: Duration::minutes(15),
            maximum_presign_seconds: 604_800,
            maximum_header_bytes: 64 * 1024,
        }
    }

    /// Enables durable S3 security auditing.
    #[must_use]
    pub fn with_audit(mut self, audit: Arc<dyn AuditRepository>) -> Self {
        self.audit = Some(audit);
        self
    }

    /// Enables centralized policy evaluation for non-root principals.
    #[must_use]
    pub fn with_authorizer(mut self, authorizer: Arc<dyn Authorizer>) -> Self {
        self.authorizer = Some(authorizer);
        self
    }

    /// Controls whether the root signing credential may access the S3 surface.
    #[must_use]
    pub const fn with_root_s3_enabled(mut self, enabled: bool) -> Self {
        self.root_s3_enabled = enabled;
        self
    }

    /// Applies the maximum lifetime accepted for presigned URLs.
    #[must_use]
    pub const fn with_maximum_presign_seconds(mut self, seconds: i64) -> Self {
        self.maximum_presign_seconds = seconds;
        self
    }

    /// Applies the resolved aggregate S3 header limit.
    #[must_use]
    pub const fn with_maximum_header_bytes(mut self, maximum_header_bytes: usize) -> Self {
        self.maximum_header_bytes = maximum_header_bytes;
        self
    }
}

/// Builds the supported S3 API router.
pub fn router(state: S3State) -> Router {
    Router::new()
        .route("/", get(list_buckets))
        .route(
            "/{bucket}",
            put(create_bucket)
                .head(head_bucket)
                .delete(delete_bucket)
                .get(list_objects_v2),
        )
        .route(
            "/{bucket}/",
            put(create_bucket)
                .head(head_bucket)
                .delete(delete_bucket)
                .get(list_objects_v2),
        )
        .route(
            "/{bucket}/{*key}",
            put(put_object)
                .post(post_object)
                .get(get_object)
                .head(head_object)
                .delete(delete_object),
        )
        .fallback(unsupported_operation)
        .method_not_allowed_fallback(unsupported_operation)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            authenticate_request,
        ))
        .with_state(state)
}

async fn authenticate_request(
    State(state): State<S3State>,
    mut request: Request,
    next: Next,
) -> Response {
    let started = Instant::now();
    let request_id = S3RequestId::new();
    request.extensions_mut().insert(request_id.clone());
    let method = request.method().clone();
    let uri = request.uri().clone();
    let audit_resource = uri.path().to_owned();
    let source_ip = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|connect| connect.0.ip().to_string());
    let headers = request.headers().clone();
    let header_bytes = headers.iter().fold(0_usize, |total, (name, value)| {
        total
            .saturating_add(name.as_str().len())
            .saturating_add(value.as_bytes().len())
    });
    if header_bytes > state.maximum_header_bytes {
        let mut response = S3Error::new(
            S3ErrorKind::InvalidRequest,
            request_id.clone(),
            request.uri().path(),
        )
        .into_response();
        insert_request_id(&mut response, &request_id);
        append_s3_audit(
            &state,
            &request_id,
            &method,
            &audit_resource,
            None,
            source_ip.clone(),
            response.status(),
        )
        .await;
        return response;
    }
    let authorization = match verify_request(&state, method.clone(), uri, headers).await {
        Ok(authenticated) => {
            let permissions = request_permissions(&request);
            match permissions {
                Err(kind) => Err(kind),
                Ok(_)
                    if !state.root_s3_enabled
                        && matches!(authenticated.principal, Principal::System { ref component } if component == "root") =>
                {
                    Err(S3ErrorKind::AccessDenied)
                }
                Ok(permissions) => {
                    authorize_permissions(&state, &authenticated.principal, permissions)
                        .await
                        .map(|()| authenticated)
                }
            }
        }
        Err(kind) => Err(kind),
    };
    let mut audit_principal = None;
    let mut response = match authorization {
        Ok(authenticated) => {
            audit_principal = Some(authenticated.principal.clone());
            request.extensions_mut().insert(authenticated.principal);
            request.extensions_mut().insert(authenticated.payload);
            next.run(request).await
        }
        Err(kind) => S3Error::new(kind, request_id.clone(), request.uri().path()).into_response(),
    };
    insert_request_id(&mut response, &request_id);
    append_s3_audit(
        &state,
        &request_id,
        &method,
        &audit_resource,
        audit_principal.as_ref(),
        source_ip,
        response.status(),
    )
    .await;
    let duration_micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
    info!(
        request_id = %request_id.0,
        method = %method,
        status = response.status().as_u16(),
        duration_micros,
        "S3 request completed"
    );
    response
}

async fn append_s3_audit(
    state: &S3State,
    request_id: &S3RequestId,
    method: &Method,
    resource: &str,
    principal: Option<&Principal>,
    source_ip: Option<String>,
    status: StatusCode,
) {
    let Some(audit) = &state.audit else { return };
    let (principal, credential_id) = principal.map_or_else(
        || ("anonymous".to_owned(), None),
        |principal| match principal {
            Principal::ServiceAccount {
                id, credential_id, ..
            } => (format!("service_account:{id}"), *credential_id),
            Principal::System { component } => (format!("system:{component}"), None),
            Principal::Anonymous => ("anonymous".into(), None),
        },
    );
    let result = match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => AuditResult::Denied,
        status if status.is_success() => AuditResult::Success,
        _ => AuditResult::Failure,
    };
    let event = AuditEvent {
        event_id: oes_core::AuditEventId::new(),
        timestamp: Utc::now(),
        request_id: Some(request_id.0.clone()),
        principal,
        credential_id,
        source_ip,
        operation: format!("s3:{}", method.as_str()),
        resource: resource.to_owned(),
        result,
        metadata: BTreeMap::new(),
    };
    if let Err(error) = audit.append(&event).await {
        tracing::error!(%error, request_id = %request_id.0, "durable S3 audit append failed");
    }
}

async fn verify_request(
    state: &S3State,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Authenticated, S3ErrorKind> {
    let (parsed, request_time, payload) = if headers.contains_key(header::AUTHORIZATION) {
        let authorization = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .ok_or(S3ErrorKind::AccessDenied)?;
        let parsed = ParsedAuthorization::parse(authorization)?;
        let request_time = parse_request_time(&headers)?;
        (parsed, request_time, parse_payload_hash(&headers)?)
    } else {
        if !uri
            .query()
            .unwrap_or_default()
            .split('&')
            .any(|item| item.starts_with("X-Amz-"))
        {
            return Err(S3ErrorKind::AccessDenied);
        }
        let presigned = ParsedPresign::parse(uri.query().unwrap_or_default())?;
        if presigned.algorithm != "AWS4-HMAC-SHA256" {
            return Err(S3ErrorKind::AuthorizationHeaderMalformed);
        }
        let request_time = parse_amz_date(&presigned.date)?;
        let age = Utc::now().signed_duration_since(request_time).num_seconds();
        if age < -state.allowed_clock_skew.num_seconds() {
            return Err(S3ErrorKind::RequestTimeTooSkewed);
        }
        if age > presigned.expires || presigned.expires > state.maximum_presign_seconds {
            return Err(S3ErrorKind::AccessDenied);
        }
        let parsed = ParsedAuthorization {
            access_key: presigned.access_key,
            scope_date: presigned.scope_date,
            region: presigned.region,
            service: presigned.service,
            terminal: presigned.terminal,
            signed_headers: presigned.signed_headers,
            signature: presigned.signature,
        };
        (parsed, request_time, PayloadHash::Unsigned)
    };
    if (Utc::now() - request_time).num_seconds().unsigned_abs()
        > state.allowed_clock_skew.num_seconds() as u64
        && headers.contains_key(header::AUTHORIZATION)
    {
        return Err(S3ErrorKind::RequestTimeTooSkewed);
    }
    if parsed.scope_date != request_time.format("%Y%m%d").to_string()
        || parsed.service != "s3"
        || parsed.terminal != "aws4_request"
        || parsed.region.is_empty()
    {
        return Err(S3ErrorKind::AuthorizationHeaderMalformed);
    }
    if method == Method::PUT
        && matches!(&payload, PayloadHash::Unsigned)
        && headers.contains_key(header::AUTHORIZATION)
    {
        return Err(S3ErrorKind::InvalidRequest);
    }
    let (principal, secret) = state
        .credentials
        .signing_secret(&parsed.access_key)
        .await
        .map_err(|error| match error {
            CredentialLookupError::UnknownAccessKey => S3ErrorKind::InvalidAccessKeyId,
            CredentialLookupError::Inactive => S3ErrorKind::AccessDenied,
            CredentialLookupError::Backend => S3ErrorKind::InternalError,
        })?;
    let canonical = canonical_request(
        &method,
        &uri,
        &headers,
        &parsed.signed_headers,
        payload.canonical_value(),
    )?;
    let canonical_hash = hex::encode(Sha256::digest(canonical.as_bytes()));
    let scope = format!(
        "{}/{}/{}/{}",
        parsed.scope_date, parsed.region, parsed.service, parsed.terminal
    );
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        request_time.format("%Y%m%dT%H%M%SZ"),
        scope,
        canonical_hash
    );
    let expected = calculate_signature(
        &secret,
        &parsed.scope_date,
        &parsed.region,
        &parsed.service,
        string_to_sign.as_bytes(),
    )?;
    let supplied =
        hex::decode(&parsed.signature).map_err(|_| S3ErrorKind::AuthorizationHeaderMalformed)?;
    if !bool::from(expected.as_slice().ct_eq(&supplied)) {
        return Err(S3ErrorKind::SignatureDoesNotMatch);
    }
    Ok(Authenticated { principal, payload })
}

async fn authorize_permissions(
    state: &S3State,
    principal: &Principal,
    permissions: Vec<Permission>,
) -> Result<(), S3ErrorKind> {
    if matches!(principal, Principal::System { .. }) {
        return Ok(());
    }
    let authorizer = state.authorizer.as_ref().ok_or(S3ErrorKind::AccessDenied)?;
    for permission in permissions {
        authorizer
            .authorize(AuthorizationContext {
                principal,
                permission: &permission,
            })
            .await
            .map_err(|_| S3ErrorKind::AccessDenied)?;
    }
    Ok(())
}

fn request_permissions(request: &Request) -> Result<Vec<Permission>, S3ErrorKind> {
    let decoded = String::from_utf8(percent_decode_str(request.uri().path()).collect())
        .map_err(|_| S3ErrorKind::InvalidRequest)?;
    let path = decoded.trim_start_matches('/');
    let path = path
        .strip_suffix('/')
        .filter(|without_slash| !without_slash.contains('/'))
        .unwrap_or(path);
    if path.is_empty() {
        return Ok(vec![Permission {
            action: Action::ListBucket,
            resource: "bucket:*".into(),
        }]);
    }
    let (bucket, key) = path
        .split_once('/')
        .map_or((path, None), |(bucket, key)| (bucket, Some(key)));
    let query = query_map(request.uri().query())?;
    let action = if key.is_none() {
        if request.method() == Method::GET && !query.contains_key("versioning") {
            Action::ListBucket
        } else {
            Action::ManageBucket
        }
    } else if request.method() == Method::GET || request.method() == Method::HEAD {
        if query.contains_key("versionId") {
            Action::GetObjectVersion
        } else {
            Action::GetObject
        }
    } else if request.method() == Method::DELETE {
        if query.contains_key("versionId") {
            Action::DeleteObjectVersion
        } else {
            Action::DeleteObject
        }
    } else {
        Action::PutObject
    };
    let resource = key.map_or_else(
        || format!("bucket:{bucket}"),
        |key| format!("bucket:{bucket}/{key}"),
    );
    let mut permissions = vec![Permission { action, resource }];
    if let Some(source) = request
        .headers()
        .get("x-amz-copy-source")
        .and_then(|value| value.to_str().ok())
    {
        let source = String::from_utf8(percent_decode_str(source).collect())
            .map_err(|_| S3ErrorKind::InvalidRequest)?;
        let source = source
            .trim_start_matches('/')
            .split_once('?')
            .map_or(source.trim_start_matches('/'), |(path, _)| path);
        permissions.push(Permission {
            action: Action::GetObject,
            resource: format!("bucket:{source}"),
        });
    }
    Ok(permissions)
}

struct ParsedPresign {
    algorithm: String,
    access_key: String,
    scope_date: String,
    region: String,
    service: String,
    terminal: String,
    date: String,
    expires: i64,
    signed_headers: Vec<String>,
    signature: String,
}

impl ParsedPresign {
    fn parse(query: &str) -> Result<Self, S3ErrorKind> {
        let mut values = BTreeMap::new();
        for item in query.split('&').filter(|item| !item.is_empty()) {
            let (name, value) = item.split_once('=').unwrap_or((item, ""));
            let name = decode_query_component(name)?;
            let value = decode_query_component(value)?;
            if name.starts_with("X-Amz-") && values.insert(name, value).is_some() {
                return Err(S3ErrorKind::AuthorizationHeaderMalformed);
            }
        }
        let algorithm = values
            .remove("X-Amz-Algorithm")
            .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?;
        let credential = values
            .remove("X-Amz-Credential")
            .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?;
        let date = values
            .remove("X-Amz-Date")
            .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?;
        let expires = values
            .remove("X-Amz-Expires")
            .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?
            .parse::<i64>()
            .map_err(|_| S3ErrorKind::AuthorizationHeaderMalformed)?;
        if !(1..=604_800).contains(&expires) {
            return Err(S3ErrorKind::InvalidRequest);
        }
        let signed_headers = values
            .remove("X-Amz-SignedHeaders")
            .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?
            .split(';')
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let signature = values
            .remove("X-Amz-Signature")
            .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?;
        if values
            .remove("X-Amz-Content-Sha256")
            .is_some_and(|value| value != "UNSIGNED-PAYLOAD")
        {
            return Err(S3ErrorKind::AuthorizationHeaderMalformed);
        }
        if !values.is_empty()
            || signed_headers.is_empty()
            || !signed_headers.windows(2).all(|pair| pair[0] < pair[1])
            || !signed_headers.iter().any(|name| name == "host")
            || signed_headers
                .iter()
                .any(|name| name.is_empty() || name.bytes().any(|byte| byte.is_ascii_uppercase()))
            || signature.len() != 64
            || !signature.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(S3ErrorKind::AuthorizationHeaderMalformed);
        }
        let mut scope = credential.split('/');
        let access_key = scope.next().filter(|value| !value.is_empty());
        let scope_date = scope.next();
        let region = scope.next();
        let service = scope.next();
        let terminal = scope.next();
        if scope.next().is_some() {
            return Err(S3ErrorKind::AuthorizationHeaderMalformed);
        }
        Ok(Self {
            algorithm,
            access_key: access_key
                .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?
                .to_owned(),
            scope_date: scope_date
                .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?
                .to_owned(),
            region: region
                .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?
                .to_owned(),
            service: service
                .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?
                .to_owned(),
            terminal: terminal
                .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?
                .to_owned(),
            date,
            expires,
            signed_headers,
            signature: signature.to_ascii_lowercase(),
        })
    }
}

#[derive(Clone)]
struct S3RequestId(String);

impl S3RequestId {
    fn new() -> Self {
        Self(Uuid::new_v4().simple().to_string())
    }
}

struct Authenticated {
    principal: Principal,
    payload: PayloadHash,
}

#[derive(Clone)]
enum PayloadHash {
    Sha256(Checksum),
    Unsigned,
}

impl PayloadHash {
    fn canonical_value(&self) -> String {
        match self {
            Self::Sha256(checksum) => hex::encode(checksum.digest()),
            Self::Unsigned => "UNSIGNED-PAYLOAD".into(),
        }
    }

    fn expected_checksum(&self) -> Option<Checksum> {
        match self {
            Self::Sha256(checksum) => Some(checksum.clone()),
            Self::Unsigned => None,
        }
    }
}

fn parse_payload_hash(headers: &HeaderMap) -> Result<PayloadHash, S3ErrorKind> {
    let value = headers
        .get("x-amz-content-sha256")
        .and_then(|value| value.to_str().ok())
        .ok_or(S3ErrorKind::InvalidRequest)?;
    if value == "UNSIGNED-PAYLOAD" {
        return Ok(PayloadHash::Unsigned);
    }
    let digest = hex::decode(value).map_err(|_| S3ErrorKind::InvalidRequest)?;
    let digest: [u8; 32] = digest.try_into().map_err(|_| S3ErrorKind::InvalidRequest)?;
    Ok(PayloadHash::Sha256(Checksum::sha256(digest)))
}

fn request_checksum(
    headers: &HeaderMap,
    payload_hash: &PayloadHash,
) -> Result<Option<Checksum>, S3ErrorKind> {
    let signed_payload = payload_hash.expected_checksum();
    let Some(encoded) = headers.get("x-amz-checksum-sha256") else {
        return Ok(signed_payload);
    };
    let encoded = encoded.to_str().map_err(|_| S3ErrorKind::InvalidRequest)?;
    let decoded = STANDARD
        .decode(encoded)
        .map_err(|_| S3ErrorKind::InvalidRequest)?;
    let digest: [u8; 32] = decoded
        .try_into()
        .map_err(|_| S3ErrorKind::InvalidRequest)?;
    let supplied = Checksum::sha256(digest);
    if signed_payload
        .as_ref()
        .is_some_and(|signed| signed != &supplied)
    {
        return Err(S3ErrorKind::BadDigest);
    }
    Ok(Some(supplied))
}

struct ParsedAuthorization {
    access_key: String,
    scope_date: String,
    region: String,
    service: String,
    terminal: String,
    signed_headers: Vec<String>,
    signature: String,
}

impl ParsedAuthorization {
    fn parse(value: &str) -> Result<Self, S3ErrorKind> {
        let parameters = value
            .strip_prefix("AWS4-HMAC-SHA256 ")
            .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?;
        let mut credential = None;
        let mut signed_headers = None;
        let mut signature = None;
        for parameter in parameters.split(',') {
            let (name, value) = parameter
                .trim()
                .split_once('=')
                .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?;
            match name {
                "Credential" if credential.is_none() => credential = Some(value),
                "SignedHeaders" if signed_headers.is_none() => signed_headers = Some(value),
                "Signature" if signature.is_none() => signature = Some(value),
                _ => return Err(S3ErrorKind::AuthorizationHeaderMalformed),
            }
        }
        let mut credential = credential
            .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?
            .split('/');
        let access_key = credential.next().filter(|value| !value.is_empty());
        let scope_date = credential.next();
        let region = credential.next();
        let service = credential.next();
        let terminal = credential.next();
        if credential.next().is_some() {
            return Err(S3ErrorKind::AuthorizationHeaderMalformed);
        }
        let signed_headers = signed_headers
            .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?
            .split(';')
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if signed_headers.is_empty()
            || !signed_headers.windows(2).all(|pair| pair[0] < pair[1])
            || signed_headers
                .iter()
                .any(|name| name.is_empty() || name.bytes().any(|byte| byte.is_ascii_uppercase()))
            || !signed_headers.iter().any(|name| name == "host")
        {
            return Err(S3ErrorKind::AuthorizationHeaderMalformed);
        }
        let signature = signature.ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?;
        if signature.len() != 64 || !signature.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(S3ErrorKind::AuthorizationHeaderMalformed);
        }
        Ok(Self {
            access_key: access_key
                .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?
                .to_owned(),
            scope_date: scope_date
                .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?
                .to_owned(),
            region: region
                .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?
                .to_owned(),
            service: service
                .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?
                .to_owned(),
            terminal: terminal
                .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?
                .to_owned(),
            signed_headers,
            signature: signature.to_ascii_lowercase(),
        })
    }
}

fn parse_request_time(headers: &HeaderMap) -> Result<DateTime<Utc>, S3ErrorKind> {
    let value = headers
        .get("x-amz-date")
        .and_then(|value| value.to_str().ok())
        .ok_or(S3ErrorKind::AccessDenied)?;
    parse_amz_date(value)
}

fn parse_amz_date(value: &str) -> Result<DateTime<Utc>, S3ErrorKind> {
    let naive = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%SZ")
        .map_err(|_| S3ErrorKind::AuthorizationHeaderMalformed)?;
    Ok(naive.and_utc())
}

fn canonical_request(
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    signed_headers: &[String],
    payload_hash: String,
) -> Result<String, S3ErrorKind> {
    let canonical_uri = aws_encode(&percent_decode_str(uri.path()).collect::<Vec<_>>(), false);
    let canonical_query = canonical_query(uri.query().unwrap_or_default());
    let mut canonical_headers = String::new();
    for name in signed_headers {
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| S3ErrorKind::AuthorizationHeaderMalformed)?;
        let values = headers.get_all(header_name);
        if values.iter().next().is_none() {
            return Err(S3ErrorKind::AuthorizationHeaderMalformed);
        }
        let mut joined = Vec::new();
        for value in values {
            let value = value
                .to_str()
                .map_err(|_| S3ErrorKind::AuthorizationHeaderMalformed)?;
            joined.push(collapse_whitespace(value));
        }
        canonical_headers.push_str(name);
        canonical_headers.push(':');
        canonical_headers.push_str(&joined.join(","));
        canonical_headers.push('\n');
    }
    Ok(format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method.as_str(),
        canonical_uri,
        canonical_query,
        canonical_headers,
        signed_headers.join(";"),
        payload_hash
    ))
}

fn canonical_query(query: &str) -> String {
    let mut pairs = query
        .split('&')
        .filter(|item| !item.is_empty())
        .map(|item| {
            let (name, value) = item.split_once('=').unwrap_or((item, ""));
            (
                aws_encode(&percent_decode_str(name).collect::<Vec<_>>(), true),
                aws_encode(&percent_decode_str(value).collect::<Vec<_>>(), true),
            )
        })
        .filter(|(name, _)| name != "X-Amz-Signature")
        .collect::<Vec<_>>();
    pairs.sort();
    pairs
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn aws_encode(value: &[u8], encode_slash: bool) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'~')
            || (!encode_slash && *byte == b'/')
        {
            encoded.push(char::from(*byte));
        } else {
            encoded.push('%');
            encoded.push_str(&format!("{byte:02X}"));
        }
    }
    encoded
}

fn collapse_whitespace(value: &str) -> String {
    value.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
}

fn calculate_signature(
    secret: &SigningSecret,
    date: &str,
    region: &str,
    service: &str,
    string_to_sign: &[u8],
) -> Result<Vec<u8>, S3ErrorKind> {
    let mut initial = b"AWS4".to_vec();
    initial.extend_from_slice(secret.expose());
    let date_key = hmac_sha256(&initial, date.as_bytes())?;
    let region_key = hmac_sha256(&date_key, region.as_bytes())?;
    let service_key = hmac_sha256(&region_key, service.as_bytes())?;
    let signing_key = hmac_sha256(&service_key, b"aws4_request")?;
    hmac_sha256(&signing_key, string_to_sign)
}

fn hmac_sha256(key: &[u8], value: &[u8]) -> Result<Vec<u8>, S3ErrorKind> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|_| S3ErrorKind::InternalError)?;
    mac.update(value);
    Ok(mac.finalize().into_bytes().to_vec())
}

async fn list_buckets(
    State(state): State<S3State>,
    RawQuery(raw_query): RawQuery,
    Extension(request_id): Extension<S3RequestId>,
) -> Result<Response, S3Error> {
    reject_subresources(raw_query.as_deref(), &request_id, "/")?;
    let buckets = state
        .services
        .buckets
        .list()
        .await
        .map_err(|error| service_error(error, request_id.clone(), "/"))?;
    let document = ListBucketsResult {
        xmlns: "http://s3.amazonaws.com/doc/2006-03-01/",
        owner: Owner {
            id: "root",
            display_name: "root",
        },
        buckets: Buckets {
            bucket: buckets
                .into_iter()
                .map(|bucket| BucketEntry {
                    name: bucket.name.to_string(),
                    creation_date: bucket.created_at.to_rfc3339(),
                })
                .collect(),
        },
    };
    xml_response(StatusCode::OK, &document, request_id, "/")
}

async fn create_bucket(
    State(state): State<S3State>,
    Path(bucket): Path<String>,
    RawQuery(raw_query): RawQuery,
    Extension(request_id): Extension<S3RequestId>,
    body: Body,
) -> Result<Response, S3Error> {
    if has_query_flag(raw_query.as_deref(), "versioning") {
        return put_bucket_versioning(state, bucket, request_id, body).await;
    }
    reject_subresources(raw_query.as_deref(), &request_id, &format!("/{bucket}"))?;
    let name = bucket_name(&bucket, &request_id)?;
    state
        .services
        .buckets
        .create(name)
        .await
        .map_err(|error| service_error(error, request_id.clone(), &format!("/{bucket}")))?;
    let mut response = StatusCode::OK.into_response();
    if let Ok(location) = HeaderValue::from_str(&format!("/{bucket}")) {
        response.headers_mut().insert(header::LOCATION, location);
    }
    Ok(response)
}

async fn head_bucket(
    State(state): State<S3State>,
    Path(bucket): Path<String>,
    RawQuery(raw_query): RawQuery,
    Extension(request_id): Extension<S3RequestId>,
) -> Result<StatusCode, S3Error> {
    reject_subresources(raw_query.as_deref(), &request_id, &format!("/{bucket}"))?;
    let name = bucket_name(&bucket, &request_id)?;
    state
        .services
        .buckets
        .head(&name)
        .await
        .map_err(|error| service_error(error, request_id, &format!("/{bucket}")))?;
    Ok(StatusCode::OK)
}

async fn delete_bucket(
    State(state): State<S3State>,
    Path(bucket): Path<String>,
    RawQuery(raw_query): RawQuery,
    Extension(request_id): Extension<S3RequestId>,
) -> Result<StatusCode, S3Error> {
    reject_subresources(raw_query.as_deref(), &request_id, &format!("/{bucket}"))?;
    let name = bucket_name(&bucket, &request_id)?;
    state
        .services
        .buckets
        .delete(&name)
        .await
        .map_err(|error| service_error(error, request_id, &format!("/{bucket}")))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn put_object(
    State(state): State<S3State>,
    Path((bucket, key)): Path<(String, String)>,
    RawQuery(raw_query): RawQuery,
    Extension(request_id): Extension<S3RequestId>,
    Extension(payload_hash): Extension<PayloadHash>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, S3Error> {
    let query = query_map(raw_query.as_deref())
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &format!("/{bucket}/{key}")))?;
    let expected_checksum = request_checksum(&headers, &payload_hash)
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &format!("/{bucket}/{key}")))?;
    if let (Some(upload_id), Some(part_number)) = (query.get("uploadId"), query.get("partNumber")) {
        let bucket_name = bucket_name(&bucket, &request_id)?;
        let object_key = object_key(&key, &request_id, &format!("/{bucket}/{key}"))?;
        let upload_id = upload_id.parse::<UploadId>().map_err(|_| {
            S3Error::new(
                S3ErrorKind::NoSuchUpload,
                request_id.clone(),
                &format!("/{bucket}/{key}"),
            )
        })?;
        let number = part_number.parse::<PartNumber>().map_err(|_| {
            S3Error::new(
                S3ErrorKind::InvalidRequest,
                request_id.clone(),
                &format!("/{bucket}/{key}"),
            )
        })?;
        if headers.contains_key("x-amz-copy-source") {
            return Err(S3Error::new(
                S3ErrorKind::NotImplemented,
                request_id,
                &format!("/{bucket}/{key}"),
            ));
        }
        let stream = body.into_data_stream().map_err(io::Error::other);
        let part = state
            .services
            .objects
            .upload_part(ServiceUploadPartRequest {
                bucket: bucket_name,
                key: object_key,
                upload_id,
                number,
                expected_checksum: expected_checksum.clone(),
                body: upload_stream(stream),
            })
            .await
            .map_err(|error| {
                service_error(error, request_id.clone(), &format!("/{bucket}/{key}"))
            })?;
        let mut response = StatusCode::OK.into_response();
        if let Ok(value) = HeaderValue::from_str(&format!("\"{}\"", part.etag)) {
            response.headers_mut().insert(header::ETAG, value);
        }
        return Ok(response);
    }
    if headers.contains_key("x-amz-copy-source") {
        return copy_object(state, bucket, key, request_id, headers).await;
    }
    reject_subresources(
        raw_query.as_deref(),
        &request_id,
        &format!("/{bucket}/{key}"),
    )?;
    if unsupported_put_headers(&headers) {
        return Err(S3Error::new(
            S3ErrorKind::NotImplemented,
            request_id,
            &format!("/{bucket}/{key}"),
        ));
    }
    let bucket_name = bucket_name(&bucket, &request_id)?;
    let object_key = object_key(&key, &request_id, &format!("/{bucket}/{key}"))?;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let custom_metadata = custom_metadata(&headers, &request_id, &format!("/{bucket}/{key}"))?;
    let stream = body.into_data_stream().map_err(io::Error::other);
    let result = state
        .services
        .objects
        .put(ServicePutRequest {
            bucket: bucket_name,
            key: object_key,
            content_type,
            custom_metadata,
            expected_checksum,
            body: upload_stream(stream),
        })
        .await
        .map_err(|error| service_error(error, request_id.clone(), &format!("/{bucket}/{key}")))?;
    let mut response = StatusCode::OK.into_response();
    insert_etag(&mut response, &result.metadata);
    insert_version_id(&mut response, result.metadata.version_id);
    Ok(response)
}

async fn get_object(
    State(state): State<S3State>,
    Path((bucket, key)): Path<(String, String)>,
    RawQuery(raw_query): RawQuery,
    Extension(request_id): Extension<S3RequestId>,
    headers: HeaderMap,
) -> Result<Response, S3Error> {
    let query = query_map(raw_query.as_deref())
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &format!("/{bucket}/{key}")))?;
    if let Some(upload_id) = query.get("uploadId") {
        return list_parts(state, bucket, key, upload_id, &query, request_id).await;
    }
    let version = requested_version(&query)
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &format!("/{bucket}/{key}")))?;
    reject_subresources(
        raw_query.as_deref(),
        &request_id,
        &format!("/{bucket}/{key}"),
    )?;
    let bucket_name = bucket_name(&bucket, &request_id)?;
    let object_key = object_key(&key, &request_id, &format!("/{bucket}/{key}"))?;
    let range = if let Some(value) = headers.get(header::RANGE) {
        let value = value
            .to_str()
            .map_err(|_| S3Error::new(S3ErrorKind::InvalidRange, request_id.clone(), &key))?;
        let metadata = match version {
            Some(RequestedVersion::Id(version_id)) => {
                state
                    .services
                    .objects
                    .head_version(&bucket_name, object_key.clone(), version_id)
                    .await
            }
            Some(RequestedVersion::Null) => {
                state
                    .services
                    .objects
                    .head_null_version(&bucket_name, object_key.clone())
                    .await
            }
            None => {
                state
                    .services
                    .objects
                    .head(&bucket_name, object_key.clone())
                    .await
            }
        }
        .map_err(|error| service_error(error, request_id.clone(), &key))?;
        Some(
            parse_range(value, metadata.size)
                .map_err(|kind| S3Error::new(kind, request_id.clone(), &key))?,
        )
    } else {
        None
    };
    let result = match version {
        Some(RequestedVersion::Id(version_id)) => {
            state
                .services
                .objects
                .get_version(&bucket_name, object_key, version_id, range)
                .await
        }
        Some(RequestedVersion::Null) => {
            state
                .services
                .objects
                .get_null_version(&bucket_name, object_key, range)
                .await
        }
        None => {
            state
                .services
                .objects
                .get(&bucket_name, object_key, range)
                .await
        }
    }
    .map_err(|error| service_error(error, request_id.clone(), &format!("/{bucket}/{key}")))?;
    conditional_streaming_response(result, &headers, request_id, &format!("/{bucket}/{key}"))
}

async fn head_object(
    State(state): State<S3State>,
    Path((bucket, key)): Path<(String, String)>,
    RawQuery(raw_query): RawQuery,
    Extension(request_id): Extension<S3RequestId>,
    headers: HeaderMap,
) -> Result<Response, S3Error> {
    let query = query_map(raw_query.as_deref())
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &format!("/{bucket}/{key}")))?;
    let version =
        requested_version(&query).map_err(|kind| S3Error::new(kind, request_id.clone(), &key))?;
    reject_subresources(
        raw_query.as_deref(),
        &request_id,
        &format!("/{bucket}/{key}"),
    )?;
    let bucket_name = bucket_name(&bucket, &request_id)?;
    let object_key = object_key(&key, &request_id, &format!("/{bucket}/{key}"))?;
    let metadata = match version {
        Some(RequestedVersion::Id(version_id)) => {
            state
                .services
                .objects
                .head_version(&bucket_name, object_key, version_id)
                .await
        }
        Some(RequestedVersion::Null) => {
            state
                .services
                .objects
                .head_null_version(&bucket_name, object_key)
                .await
        }
        None => state.services.objects.head(&bucket_name, object_key).await,
    }
    .map_err(|error| service_error(error, request_id.clone(), &format!("/{bucket}/{key}")))?;
    if evaluate_conditions(&metadata, &headers)
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &format!("/{bucket}/{key}")))?
        == ConditionalOutcome::NotModified
    {
        return Ok(StatusCode::NOT_MODIFIED.into_response());
    }
    let mut response = StatusCode::OK.into_response();
    apply_object_headers(&mut response, &metadata, metadata.size);
    insert_version_id(&mut response, metadata.version_id);
    Ok(response)
}

async fn delete_object(
    State(state): State<S3State>,
    Path((bucket, key)): Path<(String, String)>,
    RawQuery(raw_query): RawQuery,
    Extension(request_id): Extension<S3RequestId>,
) -> Result<Response, S3Error> {
    let query = query_map(raw_query.as_deref())
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &format!("/{bucket}/{key}")))?;
    if let Some(upload_id) = query.get("uploadId") {
        let bucket_name = bucket_name(&bucket, &request_id)?;
        let object_key = object_key(&key, &request_id, &key)?;
        let upload_id = upload_id
            .parse::<UploadId>()
            .map_err(|_| S3Error::new(S3ErrorKind::NoSuchUpload, request_id.clone(), &key))?;
        state
            .services
            .objects
            .abort_multipart(&bucket_name, &object_key, upload_id)
            .await
            .map_err(|error| service_error(error, request_id, &key))?;
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let version =
        requested_version(&query).map_err(|kind| S3Error::new(kind, request_id.clone(), &key))?;
    reject_subresources(
        raw_query.as_deref(),
        &request_id,
        &format!("/{bucket}/{key}"),
    )?;
    let bucket_name = bucket_name(&bucket, &request_id)?;
    let object_key = object_key(&key, &request_id, &format!("/{bucket}/{key}"))?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    match version {
        Some(RequestedVersion::Id(version_id)) => {
            state
                .services
                .objects
                .delete_version(&bucket_name, object_key, version_id)
                .await
                .map_err(|error| service_error(error, request_id, &format!("/{bucket}/{key}")))?;
            insert_version_id(&mut response, version_id);
        }
        Some(RequestedVersion::Null) => {
            state
                .services
                .objects
                .delete_null_version(&bucket_name, object_key)
                .await
                .map_err(|error| service_error(error, request_id, &format!("/{bucket}/{key}")))?;
            response.headers_mut().insert(
                HeaderName::from_static("x-amz-version-id"),
                HeaderValue::from_static("null"),
            );
        }
        None => {
            let result = state
                .services
                .objects
                .delete_detailed(&bucket_name, object_key)
                .await
                .map_err(|error| service_error(error, request_id, &format!("/{bucket}/{key}")))?;
            if let Some(marker) = result.delete_marker {
                response.headers_mut().insert(
                    HeaderName::from_static("x-amz-delete-marker"),
                    HeaderValue::from_static("true"),
                );
                insert_version_id(&mut response, marker.version_id);
            }
        }
    }
    Ok(response)
}

async fn post_object(
    State(state): State<S3State>,
    Path((bucket, key)): Path<(String, String)>,
    RawQuery(raw_query): RawQuery,
    Extension(request_id): Extension<S3RequestId>,
    Extension(payload_hash): Extension<PayloadHash>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, S3Error> {
    let query = query_map(raw_query.as_deref())
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &format!("/{bucket}/{key}")))?;
    let bucket_name = bucket_name(&bucket, &request_id)?;
    let object_key = object_key(&key, &request_id, &key)?;
    if query.contains_key("uploads") {
        if payload_hash
            .expected_checksum()
            .is_some_and(|checksum| checksum != Checksum::sha256(Sha256::digest([]).into()))
            || headers
                .get(header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .is_some_and(|length| length != 0)
        {
            return Err(S3Error::new(S3ErrorKind::BadDigest, request_id, &key));
        }
        let upload = state
            .services
            .objects
            .create_multipart(ServiceCreateMultipartRequest {
                bucket: bucket_name,
                key: object_key,
                content_type: headers
                    .get(header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned),
                custom_metadata: custom_metadata(
                    &headers,
                    &request_id,
                    &format!("/{bucket}/{key}"),
                )?,
            })
            .await
            .map_err(|error| service_error(error, request_id.clone(), &key))?;
        return xml_response(
            StatusCode::OK,
            &InitiateMultipartUploadResult {
                xmlns: "http://s3.amazonaws.com/doc/2006-03-01/",
                bucket,
                key,
                upload_id: upload.id.to_string(),
            },
            request_id,
            "/",
        );
    }
    let upload_id = query
        .get("uploadId")
        .ok_or_else(|| S3Error::new(S3ErrorKind::NotImplemented, request_id.clone(), &key))?
        .parse::<UploadId>()
        .map_err(|_| S3Error::new(S3ErrorKind::NoSuchUpload, request_id.clone(), &key))?;
    let bytes = to_bytes(body, 1024 * 1024)
        .await
        .map_err(|_| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), &key))?;
    if payload_hash
        .expected_checksum()
        .is_some_and(|expected| expected != Checksum::sha256(Sha256::digest(bytes.as_ref()).into()))
    {
        return Err(S3Error::new(S3ErrorKind::BadDigest, request_id, &key));
    }
    let document: CompleteMultipartUploadDocument = quick_xml::de::from_reader(bytes.as_ref())
        .map_err(|_| S3Error::new(S3ErrorKind::MalformedXml, request_id.clone(), &key))?;
    let manifest = document
        .parts
        .into_iter()
        .map(|part| {
            Ok(CompletedPart {
                number: PartNumber::new(part.part_number).map_err(|_| {
                    S3Error::new(S3ErrorKind::InvalidPart, request_id.clone(), &key)
                })?,
                etag: ETag::new(part.etag.trim_matches('"').to_owned()).map_err(|_| {
                    S3Error::new(S3ErrorKind::InvalidPart, request_id.clone(), &key)
                })?,
            })
        })
        .collect::<Result<Vec<_>, S3Error>>()?;
    let result = state
        .services
        .objects
        .complete_multipart(ServiceCompleteMultipartRequest {
            bucket: bucket_name,
            key: object_key,
            upload_id,
            manifest,
        })
        .await
        .map_err(|error| service_error(error, request_id.clone(), &key))?;
    let document = CompleteMultipartUploadResult {
        xmlns: "http://s3.amazonaws.com/doc/2006-03-01/",
        location: format!("/{bucket}/{key}"),
        bucket,
        key,
        etag: format!("\"{}\"", result.metadata.etag),
        version_id: result.metadata.version_id.to_string(),
    };
    xml_response(StatusCode::OK, &document, request_id, &document.location)
}

async fn put_bucket_versioning(
    state: S3State,
    bucket: String,
    request_id: S3RequestId,
    body: Body,
) -> Result<Response, S3Error> {
    let bytes = to_bytes(body, 16 * 1024)
        .await
        .map_err(|_| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), &bucket))?;
    let document: VersioningConfigurationDocument = quick_xml::de::from_reader(bytes.as_ref())
        .map_err(|_| S3Error::new(S3ErrorKind::MalformedXml, request_id.clone(), &bucket))?;
    let versioning = match document.status.as_deref() {
        Some("Enabled") => VersioningState::Enabled,
        Some("Suspended") => VersioningState::Suspended,
        _ => {
            return Err(S3Error::new(
                S3ErrorKind::InvalidRequest,
                request_id,
                &bucket,
            ));
        }
    };
    let name = bucket_name(&bucket, &request_id)?;
    state
        .services
        .buckets
        .set_versioning(&name, versioning)
        .await
        .map_err(|error| service_error(error, request_id, &bucket))?;
    Ok(StatusCode::OK.into_response())
}

async fn get_bucket_versioning(
    state: S3State,
    bucket: String,
    request_id: S3RequestId,
) -> Result<Response, S3Error> {
    let name = bucket_name(&bucket, &request_id)?;
    let bucket_record = state
        .services
        .buckets
        .head(&name)
        .await
        .map_err(|error| service_error(error, request_id.clone(), &bucket))?;
    let status = match bucket_record.versioning {
        VersioningState::Disabled => None,
        VersioningState::Enabled => Some("Enabled"),
        VersioningState::Suspended => Some("Suspended"),
    };
    xml_response(
        StatusCode::OK,
        &VersioningConfigurationResult {
            xmlns: "http://s3.amazonaws.com/doc/2006-03-01/",
            status,
        },
        request_id,
        &bucket,
    )
}

async fn copy_object(
    state: S3State,
    bucket: String,
    key: String,
    request_id: S3RequestId,
    headers: HeaderMap,
) -> Result<Response, S3Error> {
    let source = headers
        .get("x-amz-copy-source")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), &key))?;
    let decoded = String::from_utf8(percent_decode_str(source).collect())
        .map_err(|_| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), &key))?;
    let (path, source_query) = decoded.split_once('?').unwrap_or((&decoded, ""));
    let (source_bucket, source_key) = path
        .trim_start_matches('/')
        .split_once('/')
        .ok_or_else(|| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), &key))?;
    let source_version_id = query_map(Some(source_query))
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &key))?
        .get("versionId")
        .map(|value| value.parse::<VersionId>())
        .transpose()
        .map_err(|_| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), &key))?;
    let directive = match headers
        .get("x-amz-metadata-directive")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("COPY")
    {
        "COPY" => CopyMetadataDirective::Copy,
        "REPLACE" => CopyMetadataDirective::Replace,
        _ => {
            return Err(S3Error::new(S3ErrorKind::InvalidRequest, request_id, &key));
        }
    };
    let result = state
        .services
        .objects
        .copy(ServiceCopyRequest {
            source_bucket: bucket_name(source_bucket, &request_id)?,
            source_key: object_key(source_key, &request_id, source_key)?,
            source_version_id,
            destination_bucket: bucket_name(&bucket, &request_id)?,
            destination_key: object_key(&key, &request_id, &key)?,
            metadata_directive: directive,
            content_type: headers
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
            replacement_metadata: custom_metadata(&headers, &request_id, &key)?,
        })
        .await
        .map_err(|error| service_error(error, request_id.clone(), &key))?;
    xml_response(
        StatusCode::OK,
        &CopyObjectResult {
            last_modified: result.metadata.modified_at.to_rfc3339(),
            etag: format!("\"{}\"", result.metadata.etag),
            version_id: result.metadata.version_id.to_string(),
        },
        request_id,
        &key,
    )
}

async fn list_parts(
    state: S3State,
    bucket: String,
    key: String,
    upload_id: &str,
    query: &BTreeMap<String, String>,
    request_id: S3RequestId,
) -> Result<Response, S3Error> {
    let upload_id = upload_id
        .parse::<UploadId>()
        .map_err(|_| S3Error::new(S3ErrorKind::NoSuchUpload, request_id.clone(), &key))?;
    let marker = query
        .get("part-number-marker")
        .map(|value| value.parse::<PartNumber>())
        .transpose()
        .map_err(|_| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), &key))?;
    let maximum = query
        .get("max-parts")
        .map_or(Ok(1_000), |value| value.parse::<usize>())
        .map_err(|_| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), &key))?
        .min(1_000);
    let bucket_name = bucket_name(&bucket, &request_id)?;
    let object_key = object_key(&key, &request_id, &key)?;
    let parts = state
        .services
        .objects
        .list_parts(&bucket_name, &object_key, upload_id, marker, maximum + 1)
        .await
        .map_err(|error| service_error(error, request_id.clone(), &key))?;
    let truncated = parts.len() > maximum;
    let visible = parts.into_iter().take(maximum).collect::<Vec<_>>();
    let next_marker = truncated
        .then(|| visible.last().map(|part| part.number.get()))
        .flatten();
    xml_response(
        StatusCode::OK,
        &ListPartsResult {
            xmlns: "http://s3.amazonaws.com/doc/2006-03-01/",
            bucket,
            key,
            upload_id: upload_id.to_string(),
            part_number_marker: marker.map_or(0, PartNumber::get),
            next_part_number_marker: next_marker,
            max_parts: maximum,
            is_truncated: truncated,
            parts: visible
                .into_iter()
                .map(|part| ListedPart {
                    part_number: part.number.get(),
                    last_modified: part.modified_at.to_rfc3339(),
                    etag: format!("\"{}\"", part.etag),
                    size: part.size,
                })
                .collect(),
        },
        request_id,
        "/",
    )
}

async fn list_multipart_uploads(
    state: S3State,
    bucket: String,
    raw_query: Option<String>,
    request_id: S3RequestId,
) -> Result<Response, S3Error> {
    let query = query_map(raw_query.as_deref())
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &bucket))?;
    let maximum = query
        .get("max-uploads")
        .map_or(Ok(1_000), |value| value.parse::<usize>())
        .map_err(|_| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), &bucket))?
        .min(1_000);
    let marker = query
        .get("upload-id-marker")
        .map(|value| value.parse::<UploadId>())
        .transpose()
        .map_err(|_| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), &bucket))?;
    let prefix = query.get("prefix").cloned().unwrap_or_default();
    let result = state
        .services
        .objects
        .list_multipart_uploads(ServiceListMultipartUploadsRequest {
            bucket: bucket_name(&bucket, &request_id)?,
            prefix: prefix.clone(),
            upload_id_marker: marker,
            maximum_uploads: maximum,
        })
        .await
        .map_err(|error| service_error(error, request_id.clone(), &bucket))?;
    let is_truncated = result.next_upload_id_marker.is_some();
    xml_response(
        StatusCode::OK,
        &ListMultipartUploadsResult {
            xmlns: "http://s3.amazonaws.com/doc/2006-03-01/",
            bucket,
            prefix,
            upload_id_marker: marker.map(|value| value.to_string()),
            next_upload_id_marker: result.next_upload_id_marker.map(|value| value.to_string()),
            max_uploads: maximum,
            is_truncated,
            uploads: result
                .uploads
                .into_iter()
                .map(|upload| ListedUpload {
                    key: upload.key.to_string(),
                    upload_id: upload.id.to_string(),
                    initiated: upload.initiated_at.to_rfc3339(),
                })
                .collect(),
        },
        request_id,
        "/",
    )
}

async fn list_object_versions(
    state: S3State,
    bucket: String,
    raw_query: Option<String>,
    request_id: S3RequestId,
) -> Result<Response, S3Error> {
    let query = query_map(raw_query.as_deref())
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &bucket))?;
    let maximum = query
        .get("max-keys")
        .map_or(Ok(1_000), |value| value.parse::<usize>())
        .map_err(|_| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), &bucket))?
        .min(1_000);
    let key_marker = query.get("key-marker").cloned();
    let version_marker = query
        .get("version-id-marker")
        .map(|value| value.parse::<VersionId>())
        .transpose()
        .map_err(|_| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), &bucket))?;
    let prefix = query.get("prefix").cloned().unwrap_or_default();
    let result = state
        .services
        .objects
        .list_versions(ServiceListVersionsRequest {
            bucket: bucket_name(&bucket, &request_id)?,
            prefix: prefix.clone(),
            key_marker: key_marker.clone(),
            version_id_marker: version_marker,
            maximum_keys: maximum,
        })
        .await
        .map_err(|error| service_error(error, request_id.clone(), &bucket))?;
    let mut versions = Vec::new();
    let mut markers = Vec::new();
    for listed in result.versions {
        match listed.record {
            ObjectVersionRecord::Object { metadata, is_null } => versions.push(VersionEntry {
                key: metadata.key.to_string(),
                version_id: if is_null {
                    "null".into()
                } else {
                    metadata.version_id.to_string()
                },
                is_latest: listed.is_latest,
                last_modified: metadata.modified_at.to_rfc3339(),
                etag: format!("\"{}\"", metadata.etag),
                size: metadata.size,
                storage_class: "STANDARD",
            }),
            ObjectVersionRecord::DeleteMarker { marker, is_null } => {
                markers.push(DeleteMarkerEntry {
                    key: marker.key.to_string(),
                    version_id: if is_null {
                        "null".into()
                    } else {
                        marker.version_id.to_string()
                    },
                    is_latest: listed.is_latest,
                    last_modified: marker.created_at.to_rfc3339(),
                })
            }
        }
    }
    let is_truncated = result.next_key_marker.is_some();
    xml_response(
        StatusCode::OK,
        &ListVersionsResult {
            xmlns: "http://s3.amazonaws.com/doc/2006-03-01/",
            name: bucket,
            prefix,
            key_marker,
            version_id_marker: version_marker.map(|value| value.to_string()),
            next_key_marker: result.next_key_marker,
            next_version_id_marker: result.next_version_id_marker.map(|value| value.to_string()),
            max_keys: maximum,
            is_truncated,
            versions,
            delete_markers: markers,
        },
        request_id,
        "/",
    )
}

fn has_query_flag(query: Option<&str>, expected: &str) -> bool {
    query.unwrap_or_default().split('&').any(|item| {
        let name = item.split_once('=').map_or(item, |(name, _)| name);
        decode_query_component(name).is_ok_and(|name| name == expected)
    })
}

fn query_map(query: Option<&str>) -> Result<BTreeMap<String, String>, S3ErrorKind> {
    let mut values = BTreeMap::new();
    for item in query
        .unwrap_or_default()
        .split('&')
        .filter(|item| !item.is_empty())
    {
        let (name, value) = item.split_once('=').unwrap_or((item, ""));
        let name = decode_query_component(name)?;
        let value = decode_query_component(value)?;
        if values.insert(name, value).is_some() {
            return Err(S3ErrorKind::InvalidRequest);
        }
    }
    Ok(values)
}

#[derive(Debug, Clone, Copy)]
enum RequestedVersion {
    Null,
    Id(VersionId),
}

fn requested_version(
    query: &BTreeMap<String, String>,
) -> Result<Option<RequestedVersion>, S3ErrorKind> {
    query
        .get("versionId")
        .map(|value| {
            if value == "null" {
                Ok(RequestedVersion::Null)
            } else {
                value
                    .parse::<VersionId>()
                    .map(RequestedVersion::Id)
                    .map_err(|_| S3ErrorKind::InvalidRequest)
            }
        })
        .transpose()
}

#[derive(Debug, Default)]
struct ListQuery {
    list_type: Option<u8>,
    prefix: String,
    delimiter: Option<String>,
    max_keys: Option<usize>,
    continuation_token: Option<String>,
    start_after: Option<String>,
}

async fn list_objects_v2(
    State(state): State<S3State>,
    Path(bucket): Path<String>,
    RawQuery(raw_query): RawQuery,
    Extension(request_id): Extension<S3RequestId>,
) -> Result<Response, S3Error> {
    if has_query_flag(raw_query.as_deref(), "versioning") {
        return get_bucket_versioning(state, bucket, request_id).await;
    }
    if has_query_flag(raw_query.as_deref(), "versions") {
        return list_object_versions(state, bucket, raw_query, request_id).await;
    }
    if has_query_flag(raw_query.as_deref(), "uploads") {
        return list_multipart_uploads(state, bucket, raw_query, request_id).await;
    }
    let query = parse_list_query(raw_query.as_deref().unwrap_or_default())
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &format!("/{bucket}")))?;
    if query.list_type != Some(2) {
        return Err(S3Error::new(
            S3ErrorKind::NotImplemented,
            request_id,
            &format!("/{bucket}"),
        ));
    }
    let name = bucket_name(&bucket, &request_id)?;
    let continuation_start = query
        .continuation_token
        .as_deref()
        .map(decode_continuation_token)
        .transpose()
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &bucket))?;
    let start_after = continuation_start.or_else(|| query.start_after.clone());
    let delimiter = query.delimiter.filter(|value| !value.is_empty());
    let result = state
        .services
        .objects
        .list(ServiceListRequest {
            bucket: name,
            prefix: query.prefix.clone(),
            delimiter: delimiter.clone(),
            maximum_keys: query.max_keys.unwrap_or(1_000),
            start_after,
        })
        .await
        .map_err(|error| service_error(error, request_id.clone(), &bucket))?;
    let document = ListBucketResult {
        xmlns: "http://s3.amazonaws.com/doc/2006-03-01/",
        name: bucket,
        prefix: query.prefix,
        delimiter,
        key_count: result.objects.len() + result.common_prefixes.len(),
        max_keys: query.max_keys.unwrap_or(1_000),
        is_truncated: result.is_truncated,
        continuation_token: query.continuation_token,
        start_after: query.start_after,
        next_continuation_token: result.next_marker.as_deref().map(encode_continuation_token),
        contents: result
            .objects
            .into_iter()
            .map(|metadata| ObjectEntry {
                key: metadata.key.to_string(),
                last_modified: metadata.modified_at.to_rfc3339(),
                etag: format!("\"{}\"", metadata.etag),
                size: metadata.size,
                storage_class: "STANDARD",
            })
            .collect(),
        common_prefixes: result
            .common_prefixes
            .into_iter()
            .map(|prefix| CommonPrefix { prefix })
            .collect(),
    };
    xml_response(
        StatusCode::OK,
        &document,
        request_id,
        &format!("/{}", document.name),
    )
}

fn parse_list_query(query: &str) -> Result<ListQuery, S3ErrorKind> {
    let mut parsed = ListQuery::default();
    let mut seen = BTreeSet::new();
    for item in query.split('&') {
        if item.is_empty() {
            continue;
        }
        let (raw_name, raw_value) = item.split_once('=').unwrap_or((item, ""));
        let name = decode_query_component(raw_name)?;
        let value = decode_query_component(raw_value)?;
        match name.as_str() {
            "list-type" | "prefix" | "delimiter" | "max-keys" | "continuation-token"
            | "start-after" => {
                if !seen.insert(name.clone()) {
                    return Err(S3ErrorKind::InvalidRequest);
                }
            }
            _ => continue,
        }
        match name.as_str() {
            "list-type" => {
                parsed.list_type = Some(value.parse().map_err(|_| S3ErrorKind::InvalidRequest)?);
            }
            "prefix" => parsed.prefix = value,
            "delimiter" => parsed.delimiter = Some(value),
            "max-keys" => {
                parsed.max_keys = Some(value.parse().map_err(|_| S3ErrorKind::InvalidRequest)?);
            }
            "continuation-token" => parsed.continuation_token = Some(value),
            "start-after" => parsed.start_after = Some(value),
            _ => {}
        }
    }
    Ok(parsed)
}

fn decode_query_component(value: &str) -> Result<String, S3ErrorKind> {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return Err(S3ErrorKind::InvalidRequest);
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    String::from_utf8(percent_decode_str(value).collect()).map_err(|_| S3ErrorKind::InvalidRequest)
}

async fn unsupported_operation(Extension(request_id): Extension<S3RequestId>) -> S3Error {
    S3Error::new(S3ErrorKind::NotImplemented, request_id, "/")
}

fn streaming_response(result: ServiceGetResult) -> Result<Response, S3Error> {
    let length = result
        .range
        .map_or(result.metadata.size, |range| range.length);
    let status = if result.range.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let stream = result.body.map_err(io::Error::other);
    let mut response = (status, Body::from_stream(stream)).into_response();
    apply_object_headers(&mut response, &result.metadata, length);
    if let Some(range) = result.range
        && let Ok(value) = HeaderValue::from_str(&format!(
            "bytes {}-{}/{}",
            range.offset,
            range.offset + range.length - 1,
            result.metadata.size
        ))
    {
        response.headers_mut().insert(header::CONTENT_RANGE, value);
    }
    Ok(response)
}

fn conditional_streaming_response(
    result: ServiceGetResult,
    headers: &HeaderMap,
    request_id: S3RequestId,
    resource: &str,
) -> Result<Response, S3Error> {
    if evaluate_conditions(&result.metadata, headers)
        .map_err(|kind| S3Error::new(kind, request_id, resource))?
        == ConditionalOutcome::NotModified
    {
        return Ok(StatusCode::NOT_MODIFIED.into_response());
    }
    let version_id = result.metadata.version_id;
    let mut response = streaming_response(result)?;
    insert_version_id(&mut response, version_id);
    Ok(response)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConditionalOutcome {
    Proceed,
    NotModified,
}

fn evaluate_conditions(
    metadata: &ObjectMetadata,
    headers: &HeaderMap,
) -> Result<ConditionalOutcome, S3ErrorKind> {
    let etag_matches = |value: &str| {
        value == "*"
            || value
                .split(',')
                .map(str::trim)
                .map(|value| value.trim_matches('"'))
                .any(|value| value == metadata.etag.as_str())
    };
    if let Some(value) = headers.get(header::IF_MATCH) {
        let value = value.to_str().map_err(|_| S3ErrorKind::InvalidRequest)?;
        if !etag_matches(value) {
            return Err(S3ErrorKind::PreconditionFailed);
        }
    } else if let Some(value) = headers.get(header::IF_UNMODIFIED_SINCE) {
        let value = value.to_str().map_err(|_| S3ErrorKind::InvalidRequest)?;
        let time = DateTime::parse_from_rfc2822(value)
            .map_err(|_| S3ErrorKind::InvalidRequest)?
            .with_timezone(&Utc);
        if metadata.modified_at > time {
            return Err(S3ErrorKind::PreconditionFailed);
        }
    }
    if let Some(value) = headers.get(header::IF_NONE_MATCH) {
        let value = value.to_str().map_err(|_| S3ErrorKind::InvalidRequest)?;
        if etag_matches(value) {
            return Ok(ConditionalOutcome::NotModified);
        }
    } else if let Some(value) = headers.get(header::IF_MODIFIED_SINCE) {
        let value = value.to_str().map_err(|_| S3ErrorKind::InvalidRequest)?;
        let time = DateTime::parse_from_rfc2822(value)
            .map_err(|_| S3ErrorKind::InvalidRequest)?
            .with_timezone(&Utc);
        if metadata.modified_at <= time {
            return Ok(ConditionalOutcome::NotModified);
        }
    }
    Ok(ConditionalOutcome::Proceed)
}

fn apply_object_headers(response: &mut Response, metadata: &ObjectMetadata, length: u64) {
    if let Ok(value) = HeaderValue::from_str(&length.to_string()) {
        response.headers_mut().insert(header::CONTENT_LENGTH, value);
    }
    let content_type = metadata
        .content_type
        .as_deref()
        .unwrap_or("application/octet-stream");
    if let Ok(value) = HeaderValue::from_str(content_type) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    insert_etag(response, metadata);
    let modified = metadata
        .modified_at
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    if let Ok(value) = HeaderValue::from_str(&modified) {
        response.headers_mut().insert(header::LAST_MODIFIED, value);
    }
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    for (name, value) in &metadata.custom_metadata {
        let Ok(name) = HeaderName::from_bytes(format!("x-amz-meta-{name}").as_bytes()) else {
            continue;
        };
        if let Ok(value) = HeaderValue::from_str(value) {
            response.headers_mut().insert(name, value);
        }
    }
}

fn insert_etag(response: &mut Response, metadata: &ObjectMetadata) {
    if let Ok(value) = HeaderValue::from_str(&format!("\"{}\"", metadata.etag)) {
        response.headers_mut().insert(header::ETAG, value);
    }
}

fn insert_version_id(response: &mut Response, version_id: VersionId) {
    if let Ok(value) = HeaderValue::from_str(&version_id.to_string()) {
        response
            .headers_mut()
            .insert(HeaderName::from_static("x-amz-version-id"), value);
    }
}

fn custom_metadata(
    headers: &HeaderMap,
    request_id: &S3RequestId,
    resource: &str,
) -> Result<BTreeMap<String, String>, S3Error> {
    let mut metadata = BTreeMap::new();
    for (name, value) in headers {
        let Some(name) = name.as_str().strip_prefix("x-amz-meta-") else {
            continue;
        };
        if name.is_empty() {
            return Err(S3Error::new(
                S3ErrorKind::InvalidRequest,
                request_id.clone(),
                resource,
            ));
        }
        let value = value
            .to_str()
            .map_err(|_| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), resource))?;
        metadata.insert(name.to_owned(), value.to_owned());
    }
    Ok(metadata)
}

fn reject_subresources(
    query: Option<&str>,
    request_id: &S3RequestId,
    resource: &str,
) -> Result<(), S3Error> {
    for item in query.unwrap_or_default().split('&') {
        if item.is_empty() {
            continue;
        }
        let raw_name = item.split_once('=').map_or(item, |(name, _)| name);
        let name = decode_query_component(raw_name)
            .map_err(|kind| S3Error::new(kind, request_id.clone(), resource))?;
        if name != "x-id"
            && name != "versionId"
            && !matches!(
                name.as_str(),
                "X-Amz-Algorithm"
                    | "X-Amz-Credential"
                    | "X-Amz-Date"
                    | "X-Amz-Expires"
                    | "X-Amz-Content-Sha256"
                    | "X-Amz-SignedHeaders"
                    | "X-Amz-Signature"
            )
        {
            return Err(S3Error::new(
                S3ErrorKind::NotImplemented,
                request_id.clone(),
                resource,
            ));
        }
    }
    Ok(())
}

fn unsupported_put_headers(headers: &HeaderMap) -> bool {
    const UNSUPPORTED: [&str; 5] = [
        "x-amz-copy-source",
        "x-amz-acl",
        "x-amz-server-side-encryption",
        "x-amz-tagging",
        "x-amz-website-redirect-location",
    ];
    UNSUPPORTED.iter().any(|name| headers.contains_key(*name))
        || headers
            .keys()
            .any(|name| name.as_str().starts_with("x-amz-object-lock-"))
        || headers
            .get("x-amz-storage-class")
            .is_some_and(|value| value != "STANDARD")
}

fn parse_range(value: &str, size: u64) -> Result<ByteRange, S3ErrorKind> {
    let range = value
        .strip_prefix("bytes=")
        .ok_or(S3ErrorKind::InvalidRange)?;
    if range.contains(',') {
        return Err(S3ErrorKind::InvalidRange);
    }
    let (start, end) = range.split_once('-').ok_or(S3ErrorKind::InvalidRange)?;
    if start.is_empty() {
        let suffix: u64 = end.parse().map_err(|_| S3ErrorKind::InvalidRange)?;
        if suffix == 0 || size == 0 {
            return Err(S3ErrorKind::InvalidRange);
        }
        let length = suffix.min(size);
        return ByteRange::new(size - length, length).map_err(|_| S3ErrorKind::InvalidRange);
    }
    let start: u64 = start.parse().map_err(|_| S3ErrorKind::InvalidRange)?;
    let length = if end.is_empty() {
        size.checked_sub(start).ok_or(S3ErrorKind::InvalidRange)?
    } else {
        let end: u64 = end.parse().map_err(|_| S3ErrorKind::InvalidRange)?;
        if end < start {
            return Err(S3ErrorKind::InvalidRange);
        }
        end.checked_sub(start)
            .and_then(|value| value.checked_add(1))
            .ok_or(S3ErrorKind::InvalidRange)?
    };
    ByteRange::new(start, length).map_err(|_| S3ErrorKind::InvalidRange)
}

fn bucket_name(value: &str, request_id: &S3RequestId) -> Result<BucketName, S3Error> {
    BucketName::new(value)
        .map_err(|_| S3Error::new(S3ErrorKind::InvalidBucketName, request_id.clone(), value))
}

fn object_key(value: &str, request_id: &S3RequestId, resource: &str) -> Result<ObjectKey, S3Error> {
    ObjectKey::new(value)
        .map_err(|_| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), resource))
}

fn encode_continuation_token(value: &str) -> String {
    URL_SAFE_NO_PAD.encode(value.as_bytes())
}

fn decode_continuation_token(value: &str) -> Result<String, S3ErrorKind> {
    let decoded = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| S3ErrorKind::InvalidRequest)?;
    String::from_utf8(decoded).map_err(|_| S3ErrorKind::InvalidRequest)
}

fn insert_request_id(response: &mut Response, request_id: &S3RequestId) {
    if let Ok(value) = HeaderValue::from_str(&request_id.0) {
        response
            .headers_mut()
            .insert(REQUEST_ID_HEADER.clone(), value);
    }
}

fn xml_response<T: Serialize>(
    status: StatusCode,
    value: &T,
    request_id: S3RequestId,
    resource: &str,
) -> Result<Response, S3Error> {
    let xml = quick_xml::se::to_string(value)
        .map_err(|_| S3Error::new(S3ErrorKind::InternalError, request_id, resource))?;
    Ok((
        status,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static(XML_CONTENT_TYPE),
        )],
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>{xml}"),
    )
        .into_response())
}

fn service_error(error: ServiceError, request_id: S3RequestId, resource: &str) -> S3Error {
    let kind = match error {
        ServiceError::BucketNotFound => S3ErrorKind::NoSuchBucket,
        ServiceError::BucketAlreadyExists => S3ErrorKind::BucketAlreadyExists,
        ServiceError::BucketNotEmpty => S3ErrorKind::BucketNotEmpty,
        ServiceError::ObjectNotFound => S3ErrorKind::NoSuchKey,
        ServiceError::DeleteMarker(_) => S3ErrorKind::NoSuchKey,
        ServiceError::MultipartUploadNotFound => S3ErrorKind::NoSuchUpload,
        ServiceError::InvalidPart => S3ErrorKind::InvalidPart,
        ServiceError::InvalidPartOrder => S3ErrorKind::InvalidPartOrder,
        ServiceError::EntityTooSmall => S3ErrorKind::EntityTooSmall,
        ServiceError::QuotaExceeded => S3ErrorKind::QuotaExceeded,
        ServiceError::Core(_) => S3ErrorKind::InvalidRequest,
        ServiceError::MetadataTooLarge | ServiceError::InvalidRequest(_) => {
            S3ErrorKind::InvalidRequest
        }
        ServiceError::Storage(oes_storage::StorageError::ChecksumMismatch { .. }) => {
            S3ErrorKind::BadDigest
        }
        ServiceError::ClusterUnavailable(_) | ServiceError::DurabilityNotMet(_) => {
            S3ErrorKind::ServiceUnavailable
        }
        ServiceError::Metadata(_)
        | ServiceError::Storage(_)
        | ServiceError::Coordination
        | ServiceError::Unavailable => S3ErrorKind::InternalError,
    };
    S3Error::new(kind, request_id, resource)
}

struct S3Error {
    kind: S3ErrorKind,
    request_id: S3RequestId,
    resource: String,
}

impl S3Error {
    fn new(kind: S3ErrorKind, request_id: S3RequestId, resource: &str) -> Self {
        Self {
            kind,
            request_id,
            resource: resource.to_owned(),
        }
    }
}

impl IntoResponse for S3Error {
    fn into_response(self) -> Response {
        let body = ErrorDocument {
            code: self.kind.code(),
            message: self.kind.message(),
            resource: &self.resource,
            request_id: &self.request_id.0,
        };
        let xml = quick_xml::se::to_string(&body).unwrap_or_else(|_| {
            "<Error><Code>InternalError</Code><Message>Internal error</Message></Error>".into()
        });
        (
            self.kind.status(),
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static(XML_CONTENT_TYPE),
            )],
            format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>{xml}"),
        )
            .into_response()
    }
}

#[derive(Debug, Clone, Copy)]
enum S3ErrorKind {
    AccessDenied,
    InvalidAccessKeyId,
    SignatureDoesNotMatch,
    AuthorizationHeaderMalformed,
    RequestTimeTooSkewed,
    NoSuchBucket,
    NoSuchKey,
    NoSuchUpload,
    BucketAlreadyExists,
    BucketNotEmpty,
    InvalidBucketName,
    InvalidRequest,
    InvalidRange,
    PreconditionFailed,
    InvalidPart,
    InvalidPartOrder,
    EntityTooSmall,
    QuotaExceeded,
    MalformedXml,
    BadDigest,
    NotImplemented,
    ServiceUnavailable,
    InternalError,
}

impl S3ErrorKind {
    const fn code(self) -> &'static str {
        match self {
            Self::AccessDenied => "AccessDenied",
            Self::InvalidAccessKeyId => "InvalidAccessKeyId",
            Self::SignatureDoesNotMatch => "SignatureDoesNotMatch",
            Self::AuthorizationHeaderMalformed => "AuthorizationHeaderMalformed",
            Self::RequestTimeTooSkewed => "RequestTimeTooSkewed",
            Self::NoSuchBucket => "NoSuchBucket",
            Self::NoSuchKey => "NoSuchKey",
            Self::NoSuchUpload => "NoSuchUpload",
            Self::BucketAlreadyExists => "BucketAlreadyExists",
            Self::BucketNotEmpty => "BucketNotEmpty",
            Self::InvalidBucketName => "InvalidBucketName",
            Self::InvalidRequest => "InvalidRequest",
            Self::InvalidRange => "InvalidRange",
            Self::PreconditionFailed => "PreconditionFailed",
            Self::InvalidPart => "InvalidPart",
            Self::InvalidPartOrder => "InvalidPartOrder",
            Self::EntityTooSmall => "EntityTooSmall",
            Self::QuotaExceeded => "QuotaExceeded",
            Self::MalformedXml => "MalformedXML",
            Self::BadDigest => "BadDigest",
            Self::NotImplemented => "NotImplemented",
            Self::ServiceUnavailable => "ServiceUnavailable",
            Self::InternalError => "InternalError",
        }
    }

    const fn message(self) -> &'static str {
        match self {
            Self::AccessDenied => "Access Denied",
            Self::InvalidAccessKeyId => "The AWS access key ID does not exist",
            Self::SignatureDoesNotMatch => "The request signature does not match",
            Self::AuthorizationHeaderMalformed => "The authorization header is malformed",
            Self::RequestTimeTooSkewed => {
                "The difference between request time and server time is too large"
            }
            Self::NoSuchBucket => "The specified bucket does not exist",
            Self::NoSuchKey => "The specified key does not exist",
            Self::NoSuchUpload => "The specified multipart upload does not exist",
            Self::BucketAlreadyExists => "The requested bucket name is not available",
            Self::BucketNotEmpty => "The bucket is not empty",
            Self::InvalidBucketName => "The specified bucket is not valid",
            Self::InvalidRequest => "Invalid Request",
            Self::InvalidRange => "The requested range is not satisfiable",
            Self::PreconditionFailed => "At least one precondition failed",
            Self::InvalidPart => "One or more specified parts could not be found",
            Self::InvalidPartOrder => "The list of parts was not in ascending order",
            Self::EntityTooSmall => "A non-final multipart part is too small",
            Self::QuotaExceeded => "The storage quota would be exceeded",
            Self::MalformedXml => "The XML document was not well formed",
            Self::BadDigest => "The Content-MD5 or checksum did not match the received data",
            Self::NotImplemented => "A requested operation is not implemented",
            Self::ServiceUnavailable => {
                "The cluster cannot currently satisfy this request; retry shortly"
            }
            Self::InternalError => "We encountered an internal error",
        }
    }

    const fn status(self) -> StatusCode {
        match self {
            Self::AccessDenied
            | Self::InvalidAccessKeyId
            | Self::SignatureDoesNotMatch
            | Self::RequestTimeTooSkewed => StatusCode::FORBIDDEN,
            Self::NoSuchBucket | Self::NoSuchKey | Self::NoSuchUpload => StatusCode::NOT_FOUND,
            Self::BucketAlreadyExists => StatusCode::CONFLICT,
            Self::BucketNotEmpty => StatusCode::CONFLICT,
            Self::InvalidRange => StatusCode::RANGE_NOT_SATISFIABLE,
            Self::PreconditionFailed => StatusCode::PRECONDITION_FAILED,
            Self::NotImplemented => StatusCode::NOT_IMPLEMENTED,
            Self::ServiceUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::InternalError => StatusCode::INTERNAL_SERVER_ERROR,
            Self::AuthorizationHeaderMalformed
            | Self::InvalidBucketName
            | Self::InvalidRequest
            | Self::InvalidPart
            | Self::InvalidPartOrder
            | Self::EntityTooSmall
            | Self::QuotaExceeded
            | Self::MalformedXml
            | Self::BadDigest => StatusCode::BAD_REQUEST,
        }
    }
}

#[derive(Serialize)]
#[serde(rename = "Error")]
struct ErrorDocument<'a> {
    #[serde(rename = "Code")]
    code: &'a str,
    #[serde(rename = "Message")]
    message: &'a str,
    #[serde(rename = "Resource")]
    resource: &'a str,
    #[serde(rename = "RequestId")]
    request_id: &'a str,
}

#[derive(Deserialize)]
#[serde(rename = "VersioningConfiguration")]
struct VersioningConfigurationDocument {
    #[serde(rename = "Status")]
    status: Option<String>,
}

#[derive(Serialize)]
#[serde(rename = "VersioningConfiguration")]
struct VersioningConfigurationResult<'a> {
    #[serde(rename = "@xmlns")]
    xmlns: &'a str,
    #[serde(rename = "Status", skip_serializing_if = "Option::is_none")]
    status: Option<&'a str>,
}

#[derive(Serialize)]
#[serde(rename = "InitiateMultipartUploadResult")]
struct InitiateMultipartUploadResult<'a> {
    #[serde(rename = "@xmlns")]
    xmlns: &'a str,
    #[serde(rename = "Bucket")]
    bucket: String,
    #[serde(rename = "Key")]
    key: String,
    #[serde(rename = "UploadId")]
    upload_id: String,
}

#[derive(Deserialize)]
#[serde(rename = "CompleteMultipartUpload")]
struct CompleteMultipartUploadDocument {
    #[serde(rename = "Part", default)]
    parts: Vec<CompletedPartDocument>,
}

#[derive(Deserialize)]
struct CompletedPartDocument {
    #[serde(rename = "PartNumber")]
    part_number: u16,
    #[serde(rename = "ETag")]
    etag: String,
}

#[derive(Serialize)]
#[serde(rename = "CompleteMultipartUploadResult")]
struct CompleteMultipartUploadResult<'a> {
    #[serde(rename = "@xmlns")]
    xmlns: &'a str,
    #[serde(rename = "Location")]
    location: String,
    #[serde(rename = "Bucket")]
    bucket: String,
    #[serde(rename = "Key")]
    key: String,
    #[serde(rename = "ETag")]
    etag: String,
    #[serde(rename = "VersionId")]
    version_id: String,
}

#[derive(Serialize)]
#[serde(rename = "CopyObjectResult")]
struct CopyObjectResult {
    #[serde(rename = "LastModified")]
    last_modified: String,
    #[serde(rename = "ETag")]
    etag: String,
    #[serde(rename = "VersionId")]
    version_id: String,
}

#[derive(Serialize)]
#[serde(rename = "ListPartsResult")]
struct ListPartsResult<'a> {
    #[serde(rename = "@xmlns")]
    xmlns: &'a str,
    #[serde(rename = "Bucket")]
    bucket: String,
    #[serde(rename = "Key")]
    key: String,
    #[serde(rename = "UploadId")]
    upload_id: String,
    #[serde(rename = "PartNumberMarker")]
    part_number_marker: u16,
    #[serde(
        rename = "NextPartNumberMarker",
        skip_serializing_if = "Option::is_none"
    )]
    next_part_number_marker: Option<u16>,
    #[serde(rename = "MaxParts")]
    max_parts: usize,
    #[serde(rename = "IsTruncated")]
    is_truncated: bool,
    #[serde(rename = "Part", default)]
    parts: Vec<ListedPart>,
}

#[derive(Serialize)]
struct ListedPart {
    #[serde(rename = "PartNumber")]
    part_number: u16,
    #[serde(rename = "LastModified")]
    last_modified: String,
    #[serde(rename = "ETag")]
    etag: String,
    #[serde(rename = "Size")]
    size: u64,
}

#[derive(Serialize)]
#[serde(rename = "ListMultipartUploadsResult")]
struct ListMultipartUploadsResult<'a> {
    #[serde(rename = "@xmlns")]
    xmlns: &'a str,
    #[serde(rename = "Bucket")]
    bucket: String,
    #[serde(rename = "Prefix")]
    prefix: String,
    #[serde(rename = "UploadIdMarker", skip_serializing_if = "Option::is_none")]
    upload_id_marker: Option<String>,
    #[serde(rename = "NextUploadIdMarker", skip_serializing_if = "Option::is_none")]
    next_upload_id_marker: Option<String>,
    #[serde(rename = "MaxUploads")]
    max_uploads: usize,
    #[serde(rename = "IsTruncated")]
    is_truncated: bool,
    #[serde(rename = "Upload", default)]
    uploads: Vec<ListedUpload>,
}

#[derive(Serialize)]
struct ListedUpload {
    #[serde(rename = "Key")]
    key: String,
    #[serde(rename = "UploadId")]
    upload_id: String,
    #[serde(rename = "Initiated")]
    initiated: String,
}

#[derive(Serialize)]
#[serde(rename = "ListVersionsResult")]
struct ListVersionsResult<'a> {
    #[serde(rename = "@xmlns")]
    xmlns: &'a str,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Prefix")]
    prefix: String,
    #[serde(rename = "KeyMarker", skip_serializing_if = "Option::is_none")]
    key_marker: Option<String>,
    #[serde(rename = "VersionIdMarker", skip_serializing_if = "Option::is_none")]
    version_id_marker: Option<String>,
    #[serde(rename = "NextKeyMarker", skip_serializing_if = "Option::is_none")]
    next_key_marker: Option<String>,
    #[serde(
        rename = "NextVersionIdMarker",
        skip_serializing_if = "Option::is_none"
    )]
    next_version_id_marker: Option<String>,
    #[serde(rename = "MaxKeys")]
    max_keys: usize,
    #[serde(rename = "IsTruncated")]
    is_truncated: bool,
    #[serde(rename = "Version", default)]
    versions: Vec<VersionEntry>,
    #[serde(rename = "DeleteMarker", default)]
    delete_markers: Vec<DeleteMarkerEntry>,
}

#[derive(Serialize)]
struct VersionEntry {
    #[serde(rename = "Key")]
    key: String,
    #[serde(rename = "VersionId")]
    version_id: String,
    #[serde(rename = "IsLatest")]
    is_latest: bool,
    #[serde(rename = "LastModified")]
    last_modified: String,
    #[serde(rename = "ETag")]
    etag: String,
    #[serde(rename = "Size")]
    size: u64,
    #[serde(rename = "StorageClass")]
    storage_class: &'static str,
}

#[derive(Serialize)]
struct DeleteMarkerEntry {
    #[serde(rename = "Key")]
    key: String,
    #[serde(rename = "VersionId")]
    version_id: String,
    #[serde(rename = "IsLatest")]
    is_latest: bool,
    #[serde(rename = "LastModified")]
    last_modified: String,
}

#[derive(Serialize)]
#[serde(rename = "ListAllMyBucketsResult")]
struct ListBucketsResult<'a> {
    #[serde(rename = "@xmlns")]
    xmlns: &'a str,
    #[serde(rename = "Owner")]
    owner: Owner<'a>,
    #[serde(rename = "Buckets")]
    buckets: Buckets,
}

#[derive(Serialize)]
struct Owner<'a> {
    #[serde(rename = "ID")]
    id: &'a str,
    #[serde(rename = "DisplayName")]
    display_name: &'a str,
}

#[derive(Serialize)]
struct Buckets {
    #[serde(rename = "Bucket")]
    bucket: Vec<BucketEntry>,
}

#[derive(Serialize)]
struct BucketEntry {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "CreationDate")]
    creation_date: String,
}

#[derive(Serialize)]
#[serde(rename = "ListBucketResult")]
struct ListBucketResult<'a> {
    #[serde(rename = "@xmlns")]
    xmlns: &'a str,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Prefix")]
    prefix: String,
    #[serde(rename = "Delimiter", skip_serializing_if = "Option::is_none")]
    delimiter: Option<String>,
    #[serde(rename = "KeyCount")]
    key_count: usize,
    #[serde(rename = "MaxKeys")]
    max_keys: usize,
    #[serde(rename = "IsTruncated")]
    is_truncated: bool,
    #[serde(rename = "ContinuationToken", skip_serializing_if = "Option::is_none")]
    continuation_token: Option<String>,
    #[serde(rename = "StartAfter", skip_serializing_if = "Option::is_none")]
    start_after: Option<String>,
    #[serde(
        rename = "NextContinuationToken",
        skip_serializing_if = "Option::is_none"
    )]
    next_continuation_token: Option<String>,
    #[serde(rename = "Contents")]
    contents: Vec<ObjectEntry<'a>>,
    #[serde(rename = "CommonPrefixes")]
    common_prefixes: Vec<CommonPrefix>,
}

#[derive(Serialize)]
struct ObjectEntry<'a> {
    #[serde(rename = "Key")]
    key: String,
    #[serde(rename = "LastModified")]
    last_modified: String,
    #[serde(rename = "ETag")]
    etag: String,
    #[serde(rename = "Size")]
    size: u64,
    #[serde(rename = "StorageClass")]
    storage_class: &'a str,
}

#[derive(Serialize)]
struct CommonPrefix {
    #[serde(rename = "Prefix")]
    prefix: String,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::http::Request as HttpRequest;
    use http_body_util::BodyExt;
    use oes_auth::{Action, Authorizer, CredentialManager, PolicyEffect, PolicyStatement};
    use oes_core::OrganizationId;
    use oes_metadata::{MetadataRepository, RedbMetadataRepository};
    use oes_service::ServiceLimits;
    use oes_storage::{LocalFilesystemStore, ObjectStore};
    use proptest::prelude::*;
    use tempfile::{TempDir, tempdir};
    use tower::ServiceExt;

    use super::*;

    const TEST_ACCESS_KEY: &str = "root-test-access";
    const TEST_SECRET_KEY: &str = "root-test-secret-at-least-sixteen";

    async fn test_router() -> (TempDir, Router, Arc<CredentialManager>) {
        let directory = tempdir().expect("temporary directory");
        let metadata_impl = Arc::new(
            RedbMetadataRepository::open(directory.path().join("metadata.redb"))
                .await
                .expect("metadata repository"),
        );
        let metadata: Arc<dyn MetadataRepository> = metadata_impl;
        let storage_impl = Arc::new(
            LocalFilesystemStore::open(
                directory.path().join("data"),
                directory.path().join("data/tmp"),
                Arc::clone(&metadata),
            )
            .await
            .expect("filesystem store"),
        );
        let storage: Arc<dyn ObjectStore> = storage_impl;
        let services = Services::new(
            storage,
            metadata,
            OrganizationId::new(),
            ServiceLimits {
                maximum_concurrent_operations: 16,
                maximum_custom_metadata_entries: 8,
                maximum_custom_metadata_bytes: 1_024,
            },
        );
        let credentials = Arc::new(
            CredentialManager::open(
                directory.path().join("credentials.redb"),
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                Some(b"s3-test-master-key-at-least-32-bytes"),
            )
            .await
            .expect("credential manager"),
        );
        let provider: Arc<dyn SigningCredentialProvider> = credentials.clone();
        let authorizer: Arc<dyn Authorizer> = credentials.clone();
        (
            directory,
            router(S3State::new(services, provider).with_authorizer(authorizer)),
            credentials,
        )
    }

    fn signed_request(
        method: Method,
        uri: &str,
        payload: &[u8],
        extra_headers: &[(&str, &str)],
        access_key: &str,
        secret_key: &str,
        time: DateTime<Utc>,
    ) -> HttpRequest<Body> {
        let uri: Uri = uri.parse().expect("request URI");
        let payload_hash = hex::encode(Sha256::digest(payload));
        let timestamp = time.format("%Y%m%dT%H%M%SZ").to_string();
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("localhost"));
        headers.insert(
            HeaderName::from_static("x-amz-content-sha256"),
            HeaderValue::from_str(&payload_hash).expect("payload hash header"),
        );
        headers.insert(
            HeaderName::from_static("x-amz-date"),
            HeaderValue::from_str(&timestamp).expect("timestamp header"),
        );
        for (name, value) in extra_headers {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes()).expect("header name"),
                HeaderValue::from_str(value).expect("header value"),
            );
        }
        let mut signed_headers = headers
            .keys()
            .map(|name| name.as_str().to_owned())
            .collect::<Vec<_>>();
        signed_headers.sort();
        let canonical = canonical_request(&method, &uri, &headers, &signed_headers, payload_hash)
            .expect("canonical request");
        let date = time.format("%Y%m%d").to_string();
        let scope = format!("{date}/us-east-1/s3/aws4_request");
        let canonical_hash = hex::encode(Sha256::digest(canonical.as_bytes()));
        let string_to_sign = format!("AWS4-HMAC-SHA256\n{timestamp}\n{scope}\n{canonical_hash}");
        let signature = calculate_signature(
            &SigningSecret::new(secret_key),
            &date,
            "us-east-1",
            "s3",
            string_to_sign.as_bytes(),
        )
        .expect("signature");
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!(
                "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={}, Signature={}",
                signed_headers.join(";"),
                hex::encode(signature)
            ))
            .expect("authorization header"),
        );
        let mut request = HttpRequest::builder()
            .method(method)
            .uri(uri)
            .body(Body::from(payload.to_vec()))
            .expect("HTTP request");
        *request.headers_mut() = headers;
        request
    }

    fn presigned_request(
        method: Method,
        path: &str,
        access_key: &str,
        secret_key: &str,
        time: DateTime<Utc>,
        expires: i64,
    ) -> HttpRequest<Body> {
        let date = time.format("%Y%m%d").to_string();
        let timestamp = time.format("%Y%m%dT%H%M%SZ").to_string();
        let scope = format!("{date}/us-east-1/s3/aws4_request");
        let credential = format!("{access_key}/{scope}");
        let query = format!(
            "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential={}&X-Amz-Date={timestamp}&X-Amz-Expires={expires}&X-Amz-SignedHeaders=host",
            aws_encode(credential.as_bytes(), true)
        );
        let uri: Uri = format!("{path}?{query}").parse().expect("presigned URI");
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("localhost"));
        let canonical = canonical_request(
            &method,
            &uri,
            &headers,
            &["host".into()],
            "UNSIGNED-PAYLOAD".into(),
        )
        .expect("presigned canonical request");
        let canonical_hash = hex::encode(Sha256::digest(canonical.as_bytes()));
        let string_to_sign = format!("AWS4-HMAC-SHA256\n{timestamp}\n{scope}\n{canonical_hash}");
        let signature = calculate_signature(
            &SigningSecret::new(secret_key),
            &date,
            "us-east-1",
            "s3",
            string_to_sign.as_bytes(),
        )
        .expect("presigned signature");
        let uri: Uri = format!("{uri}&X-Amz-Signature={}", hex::encode(signature))
            .parse()
            .expect("signed URI");
        let mut request = HttpRequest::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .expect("HTTP request");
        *request.headers_mut() = headers;
        request
    }

    async fn body_text(response: Response) -> String {
        let body = response
            .into_body()
            .collect()
            .await
            .expect("response body")
            .to_bytes();
        String::from_utf8(body.to_vec()).expect("UTF-8 response")
    }

    fn xml_value<'a>(document: &'a str, element: &str) -> Option<&'a str> {
        let start_tag = format!("<{element}>");
        let end_tag = format!("</{element}>");
        let start = document.find(&start_tag)? + start_tag.len();
        let end = document[start..].find(&end_tag)? + start;
        Some(&document[start..end])
    }

    #[test]
    fn canonical_request_matches_aws_documentation_example() {
        let uri: Uri = "https://examplebucket.s3.amazonaws.com/test.txt"
            .parse()
            .expect("URI");
        let mut headers = HeaderMap::new();
        headers.insert(
            "host",
            HeaderValue::from_static("examplebucket.s3.amazonaws.com"),
        );
        headers.insert("range", HeaderValue::from_static("bytes=0-9"));
        headers.insert(
            "x-amz-content-sha256",
            HeaderValue::from_static(EMPTY_PAYLOAD_SHA256),
        );
        headers.insert("x-amz-date", HeaderValue::from_static("20130524T000000Z"));
        let signed = vec![
            "host".into(),
            "range".into(),
            "x-amz-content-sha256".into(),
            "x-amz-date".into(),
        ];
        let canonical = canonical_request(
            &Method::GET,
            &uri,
            &headers,
            &signed,
            EMPTY_PAYLOAD_SHA256.into(),
        )
        .expect("canonical request");
        assert_eq!(
            hex::encode(Sha256::digest(canonical.as_bytes())),
            "7344ae5b7ee6c3e7e6b0fe0640412a37625d1fbfff95c48bbb2dc43964946972"
        );
    }

    #[test]
    fn malformed_authorization_and_ranges_are_rejected() {
        assert!(ParsedAuthorization::parse("Bearer secret").is_err());
        assert!(ParsedAuthorization::parse("AWS4-HMAC-SHA256 Credential=x").is_err());
        assert!(parse_range("bytes=5-2", 10).is_err());
        assert!(parse_range("bytes=0-1,4-5", 10).is_err());
        assert_eq!(parse_range("bytes=-4", 10).expect("suffix").offset(), 6);
    }

    #[test]
    fn canonical_query_sorts_and_encodes_values() {
        assert_eq!(
            canonical_query("z=last&a=hello%20world&a=first"),
            "a=first&a=hello%20world&z=last"
        );
    }

    #[test]
    fn client_sha256_checksum_is_strictly_decoded_and_cross_checked() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-amz-checksum-sha256",
            HeaderValue::from_str(&STANDARD.encode([7_u8; 32])).expect("header"),
        );
        assert_eq!(
            request_checksum(&headers, &PayloadHash::Unsigned).expect("checksum"),
            Some(Checksum::sha256([7_u8; 32]))
        );
        assert!(matches!(
            request_checksum(&headers, &PayloadHash::Sha256(Checksum::sha256([8_u8; 32]))),
            Err(S3ErrorKind::BadDigest)
        ));
    }

    #[tokio::test]
    async fn presigned_get_put_are_bounded_to_method_and_expiration() {
        let (_directory, application, _credentials) = test_router().await;
        let now = Utc::now();
        let create = signed_request(
            Method::PUT,
            "/presigned-bucket",
            b"",
            &[],
            TEST_ACCESS_KEY,
            TEST_SECRET_KEY,
            now,
        );
        assert_eq!(
            application
                .clone()
                .oneshot(create)
                .await
                .expect("create bucket")
                .status(),
            StatusCode::OK
        );

        let mut put = presigned_request(
            Method::PUT,
            "/presigned-bucket/object.txt",
            TEST_ACCESS_KEY,
            TEST_SECRET_KEY,
            now,
            60,
        );
        *put.body_mut() = Body::from("presigned payload");
        assert_eq!(
            application
                .clone()
                .oneshot(put)
                .await
                .expect("presigned put")
                .status(),
            StatusCode::OK
        );

        let get = presigned_request(
            Method::GET,
            "/presigned-bucket/object.txt",
            TEST_ACCESS_KEY,
            TEST_SECRET_KEY,
            now,
            60,
        );
        let get_uri = get.uri().clone();
        let response = application
            .clone()
            .oneshot(get)
            .await
            .expect("presigned get");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_text(response).await, "presigned payload");

        let mut delete = HttpRequest::builder()
            .method(Method::DELETE)
            .uri(get_uri)
            .body(Body::empty())
            .expect("method-confusion request");
        delete
            .headers_mut()
            .insert(header::HOST, HeaderValue::from_static("localhost"));
        assert_eq!(
            application
                .clone()
                .oneshot(delete)
                .await
                .expect("method-bound URL")
                .status(),
            StatusCode::FORBIDDEN
        );

        let expired = presigned_request(
            Method::GET,
            "/presigned-bucket/object.txt",
            TEST_ACCESS_KEY,
            TEST_SECRET_KEY,
            now - Duration::seconds(120),
            60,
        );
        assert_eq!(
            application
                .oneshot(expired)
                .await
                .expect("expired URL")
                .status(),
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn signed_s3_lifecycle_streams_metadata_ranges_listing_and_idempotent_delete() {
        let (_directory, application, _credentials) = test_router().await;
        let now = Utc::now();

        let response = application
            .clone()
            .oneshot(signed_request(
                Method::GET,
                "/",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("list buckets response");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_text(response).await.contains("ListAllMyBucketsResult"));

        let response = application
            .clone()
            .oneshot(signed_request(
                Method::PUT,
                "/demo-bucket",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("create bucket response");
        assert_eq!(response.status(), StatusCode::OK);

        let duplicate = application
            .clone()
            .oneshot(signed_request(
                Method::PUT,
                "/demo-bucket",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("duplicate bucket response");
        assert_eq!(duplicate.status(), StatusCode::CONFLICT);
        assert_eq!(
            xml_value(&body_text(duplicate).await, "Code"),
            Some("BucketAlreadyExists")
        );

        let oversized_metadata = application
            .clone()
            .oneshot(signed_request(
                Method::PUT,
                "/demo-bucket/too-much-metadata",
                b"payload",
                &[
                    ("x-amz-meta-1", "value"),
                    ("x-amz-meta-2", "value"),
                    ("x-amz-meta-3", "value"),
                    ("x-amz-meta-4", "value"),
                    ("x-amz-meta-5", "value"),
                    ("x-amz-meta-6", "value"),
                    ("x-amz-meta-7", "value"),
                    ("x-amz-meta-8", "value"),
                    ("x-amz-meta-9", "value"),
                ],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("oversized metadata response");
        assert_eq!(oversized_metadata.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            xml_value(&body_text(oversized_metadata).await, "Code"),
            Some("InvalidRequest")
        );

        let object_uri = "/demo-bucket/users/123/profile.txt";
        let payload = b"hello world";
        let response = application
            .clone()
            .oneshot(signed_request(
                Method::PUT,
                object_uri,
                payload,
                &[
                    ("content-type", "text/plain"),
                    ("x-amz-meta-origin", "compatibility-test"),
                ],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("put object response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::ETAG),
            Some(&HeaderValue::from_static(
                "\"5eb63bbbe01eeed093cb22bb8f5acdc3\""
            ))
        );

        let copy = application
            .clone()
            .oneshot(signed_request(
                Method::PUT,
                "/demo-bucket/copied.txt",
                b"",
                &[("x-amz-copy-source", "/demo-bucket/users/123/profile.txt")],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("copy response");
        assert_eq!(copy.status(), StatusCode::OK);
        assert_eq!(
            xml_value(&body_text(copy).await, "ETag"),
            Some("\"5eb63bbbe01eeed093cb22bb8f5acdc3\"")
        );

        let head = application
            .clone()
            .oneshot(signed_request(
                Method::HEAD,
                object_uri,
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("head object response");
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(head.headers().get(header::CONTENT_LENGTH), Some(&11.into()));
        assert_eq!(
            head.headers().get(header::CONTENT_TYPE),
            Some(&HeaderValue::from_static("text/plain"))
        );
        assert_eq!(
            head.headers().get("x-amz-meta-origin"),
            Some(&HeaderValue::from_static("compatibility-test"))
        );

        let range = application
            .clone()
            .oneshot(signed_request(
                Method::GET,
                object_uri,
                b"",
                &[("range", "bytes=6-")],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("range response");
        assert_eq!(range.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            range.headers().get(header::CONTENT_RANGE),
            Some(&HeaderValue::from_static("bytes 6-10/11"))
        );
        assert_eq!(body_text(range).await, "world");

        let listing = application
            .clone()
            .oneshot(signed_request(
                Method::GET,
                "/demo-bucket?list-type=2&prefix=users%2F&delimiter=%2F&max-keys=1",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("list objects response");
        assert_eq!(listing.status(), StatusCode::OK);
        let listing = body_text(listing).await;
        assert!(listing.contains("<Prefix>users/123/</Prefix>"));
        assert!(listing.contains("<KeyCount>1</KeyCount>"));

        let non_empty = application
            .clone()
            .oneshot(signed_request(
                Method::DELETE,
                "/demo-bucket",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("non-empty bucket response");
        assert_eq!(non_empty.status(), StatusCode::CONFLICT);
        assert_eq!(
            xml_value(&body_text(non_empty).await, "Code"),
            Some("BucketNotEmpty")
        );

        let missing_multipart_delete = application
            .clone()
            .oneshot(signed_request(
                Method::DELETE,
                &format!("{object_uri}?uploadId=unsupported"),
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("missing multipart delete response");
        assert_eq!(missing_multipart_delete.status(), StatusCode::NOT_FOUND);

        for _ in 0..2 {
            let deleted = application
                .clone()
                .oneshot(signed_request(
                    Method::DELETE,
                    object_uri,
                    b"",
                    &[],
                    TEST_ACCESS_KEY,
                    TEST_SECRET_KEY,
                    now,
                ))
                .await
                .expect("delete object response");
            assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
        }

        let missing = application
            .clone()
            .oneshot(signed_request(
                Method::GET,
                object_uri,
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("missing object response");
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            xml_value(&body_text(missing).await, "Code"),
            Some("NoSuchKey")
        );

        let deleted_copy = application
            .clone()
            .oneshot(signed_request(
                Method::DELETE,
                "/demo-bucket/copied.txt",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("delete copied object response");
        assert_eq!(deleted_copy.status(), StatusCode::NO_CONTENT);

        let deleted_bucket = application
            .oneshot(signed_request(
                Method::DELETE,
                "/demo-bucket",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("delete bucket response");
        assert_eq!(deleted_bucket.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn multipart_and_versioning_work_through_signed_s3_requests() {
        let (_directory, application, _credentials) = test_router().await;
        let now = Utc::now();
        let create = application
            .clone()
            .oneshot(signed_request(
                Method::PUT,
                "/advanced-bucket",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("create bucket");
        assert_eq!(create.status(), StatusCode::OK);

        let configuration =
            b"<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>";
        let enabled = application
            .clone()
            .oneshot(signed_request(
                Method::PUT,
                "/advanced-bucket?versioning",
                configuration,
                &[("content-type", "application/xml")],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("enable versioning");
        assert_eq!(enabled.status(), StatusCode::OK);

        let first = application
            .clone()
            .oneshot(signed_request(
                Method::PUT,
                "/advanced-bucket/versioned.txt",
                b"first",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("first version");
        let first_version = first
            .headers()
            .get("x-amz-version-id")
            .expect("version header")
            .to_str()
            .expect("version text")
            .to_owned();
        assert_eq!(first.status(), StatusCode::OK);
        let second = application
            .clone()
            .oneshot(signed_request(
                Method::PUT,
                "/advanced-bucket/versioned.txt",
                b"second",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("second version");
        assert_eq!(second.status(), StatusCode::OK);
        let historical = application
            .clone()
            .oneshot(signed_request(
                Method::GET,
                &format!("/advanced-bucket/versioned.txt?versionId={first_version}"),
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("historical get");
        assert_eq!(historical.status(), StatusCode::OK);
        assert_eq!(body_text(historical).await, "first");
        let versions = application
            .clone()
            .oneshot(signed_request(
                Method::GET,
                "/advanced-bucket?versions",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("list versions");
        assert_eq!(versions.status(), StatusCode::OK);
        assert_eq!(body_text(versions).await.matches("<Version>").count(), 2);

        let initiated = application
            .clone()
            .oneshot(signed_request(
                Method::POST,
                "/advanced-bucket/multipart.bin?uploads",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("initiate multipart");
        assert_eq!(initiated.status(), StatusCode::OK);
        let upload_id = xml_value(&body_text(initiated).await, "UploadId")
            .expect("upload id")
            .to_owned();
        let part = application
            .clone()
            .oneshot(signed_request(
                Method::PUT,
                &format!("/advanced-bucket/multipart.bin?partNumber=1&uploadId={upload_id}"),
                b"streamed-part",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("upload part");
        assert_eq!(part.status(), StatusCode::OK);
        let etag = part
            .headers()
            .get(header::ETAG)
            .expect("part ETag")
            .to_str()
            .expect("ETag text")
            .to_owned();
        let completion = format!(
            "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{etag}</ETag></Part></CompleteMultipartUpload>"
        );
        let completed = application
            .clone()
            .oneshot(signed_request(
                Method::POST,
                &format!("/advanced-bucket/multipart.bin?uploadId={upload_id}"),
                completion.as_bytes(),
                &[("content-type", "application/xml")],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("complete multipart");
        assert_eq!(completed.status(), StatusCode::OK);
        let downloaded = application
            .oneshot(signed_request(
                Method::GET,
                "/advanced-bucket/multipart.bin",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("download multipart");
        assert_eq!(body_text(downloaded).await, "streamed-part");
    }

    #[tokio::test]
    async fn authentication_and_parser_failures_return_s3_xml_without_reaching_storage() {
        let (_directory, application, credentials) = test_router().await;
        let now = Utc::now();

        let unknown = application
            .clone()
            .oneshot(signed_request(
                Method::GET,
                "/",
                b"",
                &[],
                "unknown-access",
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("unknown credential response");
        assert_eq!(unknown.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            xml_value(&body_text(unknown).await, "Code"),
            Some("InvalidAccessKeyId")
        );

        let mut invalid_signature = signed_request(
            Method::GET,
            "/",
            b"",
            &[],
            TEST_ACCESS_KEY,
            TEST_SECRET_KEY,
            now,
        );
        let authorization = invalid_signature
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .expect("authorization text");
        let replacement = if authorization.ends_with('0') {
            '1'
        } else {
            '0'
        };
        let invalid_authorization =
            format!("{}{replacement}", &authorization[..authorization.len() - 1]);
        invalid_signature.headers_mut().insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&invalid_authorization).expect("invalid signature header"),
        );
        let invalid_signature = application
            .clone()
            .oneshot(invalid_signature)
            .await
            .expect("invalid signature response");
        assert_eq!(invalid_signature.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            xml_value(&body_text(invalid_signature).await, "Code"),
            Some("SignatureDoesNotMatch")
        );

        let expired = application
            .clone()
            .oneshot(signed_request(
                Method::GET,
                "/",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now - Duration::hours(1),
            ))
            .await
            .expect("expired timestamp response");
        assert_eq!(expired.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            xml_value(&body_text(expired).await, "Code"),
            Some("RequestTimeTooSkewed")
        );

        let malformed_query = application
            .clone()
            .oneshot(signed_request(
                Method::GET,
                "/missing-bucket?list-type=2&max-keys=invalid",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("malformed query response");
        assert_eq!(malformed_query.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            xml_value(&body_text(malformed_query).await, "Code"),
            Some("InvalidRequest")
        );

        let traversal = application
            .clone()
            .oneshot(signed_request(
                Method::PUT,
                "/missing-bucket/%2E%2E%2Fescape",
                b"payload",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("traversal response");
        assert!(!traversal.status().is_success());

        let issued = credentials
            .create_service_account("s3-test-client", OrganizationId::new())
            .await
            .expect("issue service account");
        let policy = credentials
            .create_policy(
                "s3-test-access",
                "test-only full access",
                vec![PolicyStatement {
                    effect: PolicyEffect::Allow,
                    actions: vec![
                        Action::ListBucket,
                        Action::GetObject,
                        Action::PutObject,
                        Action::DeleteObject,
                        Action::GetObjectVersion,
                        Action::DeleteObjectVersion,
                        Action::ManageBucket,
                    ],
                    resources: vec!["bucket:*".into()],
                }],
            )
            .await
            .expect("create policy");
        credentials
            .attach_policy(issued.info.account.id, policy.id)
            .await
            .expect("attach policy");
        let secret = std::str::from_utf8(issued.secret.expose()).expect("secret text");
        let service_account_request = application
            .clone()
            .oneshot(signed_request(
                Method::GET,
                "/",
                b"",
                &[],
                &issued.info.credential.key_id,
                secret,
                now,
            ))
            .await
            .expect("service account response");
        assert_eq!(service_account_request.status(), StatusCode::OK);

        credentials
            .revoke_service_account(issued.info.account.id)
            .await
            .expect("revoke service account");
        let revoked = application
            .oneshot(signed_request(
                Method::GET,
                "/",
                b"",
                &[],
                &issued.info.credential.key_id,
                secret,
                now,
            ))
            .await
            .expect("revoked credential response");
        assert_eq!(revoked.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            xml_value(&body_text(revoked).await, "Code"),
            Some("AccessDenied")
        );
    }

    #[tokio::test]
    async fn list_objects_v2_continuation_is_bounded_and_lossless() {
        let (_directory, application, _credentials) = test_router().await;
        let now = Utc::now();
        let created = application
            .clone()
            .oneshot(signed_request(
                Method::PUT,
                "/page-bucket",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("create bucket");
        assert_eq!(created.status(), StatusCode::OK);
        for key in ["a", "b", "c"] {
            let response = application
                .clone()
                .oneshot(signed_request(
                    Method::PUT,
                    &format!("/page-bucket/{key}"),
                    key.as_bytes(),
                    &[],
                    TEST_ACCESS_KEY,
                    TEST_SECRET_KEY,
                    now,
                ))
                .await
                .expect("put listing object");
            assert_eq!(response.status(), StatusCode::OK);
        }

        let first = application
            .clone()
            .oneshot(signed_request(
                Method::GET,
                "/page-bucket?list-type=2&max-keys=2",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("first listing page");
        assert_eq!(first.status(), StatusCode::OK);
        let first = body_text(first).await;
        assert!(first.contains("<Key>a</Key>"));
        assert!(first.contains("<Key>b</Key>"));
        assert!(first.contains("<IsTruncated>true</IsTruncated>"));
        let token = xml_value(&first, "NextContinuationToken").expect("continuation token");

        let second = application
            .oneshot(signed_request(
                Method::GET,
                &format!("/page-bucket?list-type=2&max-keys=2&continuation-token={token}"),
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("second listing page");
        assert_eq!(second.status(), StatusCode::OK);
        let second = body_text(second).await;
        assert!(second.contains("<Key>c</Key>"));
        assert!(second.contains("<IsTruncated>false</IsTruncated>"));
    }

    #[test]
    fn compatibility_registry_is_unique_and_tracks_explicit_gaps() {
        let names = S3_CAPABILITIES
            .iter()
            .map(|capability| capability.name)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(names.len(), S3_CAPABILITIES.len());
        assert!(S3_CAPABILITIES.iter().any(|capability| {
            capability.name == "MultipartUpload"
                && capability.status == CapabilityStatus::Implemented
        }));
        assert!(S3_CAPABILITIES.iter().any(|capability| {
            capability.name == "UploadPartCopy"
                && capability.status == CapabilityStatus::Unsupported
        }));
    }

    proptest! {
        #[test]
        fn risky_protocol_parsers_never_panic(
            authorization in any::<String>(),
            range in any::<String>(),
            size in any::<u64>(),
            query in any::<String>(),
        ) {
            let _ = ParsedAuthorization::parse(&authorization);
            let _ = parse_range(&range, size);
            let _ = parse_list_query(&query);
            let canonical = canonical_query(&query);
            prop_assert_eq!(canonical_query(&canonical), canonical);
        }
    }
}
