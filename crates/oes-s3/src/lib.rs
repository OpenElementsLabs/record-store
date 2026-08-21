//! S3-compatible HTTP protocol adapter.
//!
//! Supported operations are deliberately limited to bucket lifecycle,
//! single-part object lifecycle, range GET, and ListObjectsV2.

use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    sync::Arc,
    time::Instant,
};

use axum::{
    Router,
    body::Body,
    extract::{Extension, Path, RawQuery, Request, State},
    http::{
        HeaderMap, HeaderValue, Method, StatusCode, Uri,
        header::{self, HeaderName},
    },
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, put},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use futures_util::TryStreamExt;
use hmac::{Hmac, Mac};
use oes_auth::{CredentialLookupError, Principal, SigningCredentialProvider, SigningSecret};
use oes_core::{BucketName, ByteRange, Checksum, ObjectKey, ObjectMetadata};
use oes_service::{
    ServiceError, ServiceGetResult, ServiceListRequest, ServicePutRequest, Services,
};
use oes_storage::upload_stream;
use percent_encoding::percent_decode_str;
use serde::Serialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tracing::info;
use uuid::Uuid;

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
            allowed_clock_skew: Duration::minutes(15),
            maximum_presign_seconds: 604_800,
            maximum_header_bytes: 64 * 1024,
        }
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
            "/{bucket}/{*key}",
            put(put_object)
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
        return response;
    }
    let mut response = match verify_request(&state, method.clone(), uri, headers).await {
        Ok(authenticated) => {
            request.extensions_mut().insert(authenticated.principal);
            request.extensions_mut().insert(authenticated.payload);
            next.run(request).await
        }
        Err(kind) => S3Error::new(kind, request_id.clone(), request.uri().path()).into_response(),
    };
    insert_request_id(&mut response, &request_id);
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
        if age > presigned.expires || age > state.maximum_presign_seconds {
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
            if !name.starts_with("X-Amz-") || values.insert(name, value).is_some() {
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
        if !values.is_empty()
            || signed_headers != ["host"]
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
) -> Result<Response, S3Error> {
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
            expected_checksum: payload_hash.expected_checksum(),
            body: upload_stream(stream),
        })
        .await
        .map_err(|error| service_error(error, request_id, &format!("/{bucket}/{key}")))?;
    let mut response = StatusCode::OK.into_response();
    insert_etag(&mut response, &result.metadata);
    Ok(response)
}

async fn get_object(
    State(state): State<S3State>,
    Path((bucket, key)): Path<(String, String)>,
    RawQuery(raw_query): RawQuery,
    Extension(request_id): Extension<S3RequestId>,
    headers: HeaderMap,
) -> Result<Response, S3Error> {
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
        let metadata = state
            .services
            .objects
            .head(&bucket_name, object_key.clone())
            .await
            .map_err(|error| service_error(error, request_id.clone(), &key))?;
        Some(
            parse_range(value, metadata.size)
                .map_err(|kind| S3Error::new(kind, request_id.clone(), &key))?,
        )
    } else {
        None
    };
    let result = state
        .services
        .objects
        .get(&bucket_name, object_key, range)
        .await
        .map_err(|error| service_error(error, request_id, &format!("/{bucket}/{key}")))?;
    streaming_response(result)
}

async fn head_object(
    State(state): State<S3State>,
    Path((bucket, key)): Path<(String, String)>,
    RawQuery(raw_query): RawQuery,
    Extension(request_id): Extension<S3RequestId>,
) -> Result<Response, S3Error> {
    reject_subresources(
        raw_query.as_deref(),
        &request_id,
        &format!("/{bucket}/{key}"),
    )?;
    let bucket_name = bucket_name(&bucket, &request_id)?;
    let object_key = object_key(&key, &request_id, &format!("/{bucket}/{key}"))?;
    let metadata = state
        .services
        .objects
        .head(&bucket_name, object_key)
        .await
        .map_err(|error| service_error(error, request_id, &format!("/{bucket}/{key}")))?;
    let mut response = StatusCode::OK.into_response();
    apply_object_headers(&mut response, &metadata, metadata.size);
    Ok(response)
}

async fn delete_object(
    State(state): State<S3State>,
    Path((bucket, key)): Path<(String, String)>,
    RawQuery(raw_query): RawQuery,
    Extension(request_id): Extension<S3RequestId>,
) -> Result<StatusCode, S3Error> {
    reject_subresources(
        raw_query.as_deref(),
        &request_id,
        &format!("/{bucket}/{key}"),
    )?;
    let bucket_name = bucket_name(&bucket, &request_id)?;
    let object_key = object_key(&key, &request_id, &format!("/{bucket}/{key}"))?;
    state
        .services
        .objects
        .delete(&bucket_name, object_key)
        .await
        .map_err(|error| service_error(error, request_id, &format!("/{bucket}/{key}")))?;
    Ok(StatusCode::NO_CONTENT)
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
            && !matches!(
                name.as_str(),
                "X-Amz-Algorithm"
                    | "X-Amz-Credential"
                    | "X-Amz-Date"
                    | "X-Amz-Expires"
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
        ServiceError::Core(_) => S3ErrorKind::InvalidRequest,
        ServiceError::MetadataTooLarge | ServiceError::InvalidRequest(_) => {
            S3ErrorKind::InvalidRequest
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
    BucketAlreadyExists,
    BucketNotEmpty,
    InvalidBucketName,
    InvalidRequest,
    InvalidRange,
    NotImplemented,
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
            Self::BucketAlreadyExists => "BucketAlreadyExists",
            Self::BucketNotEmpty => "BucketNotEmpty",
            Self::InvalidBucketName => "InvalidBucketName",
            Self::InvalidRequest => "InvalidRequest",
            Self::InvalidRange => "InvalidRange",
            Self::NotImplemented => "NotImplemented",
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
            Self::BucketAlreadyExists => "The requested bucket name is not available",
            Self::BucketNotEmpty => "The bucket is not empty",
            Self::InvalidBucketName => "The specified bucket is not valid",
            Self::InvalidRequest => "Invalid Request",
            Self::InvalidRange => "The requested range is not satisfiable",
            Self::NotImplemented => "A requested operation is not implemented",
            Self::InternalError => "We encountered an internal error",
        }
    }

    const fn status(self) -> StatusCode {
        match self {
            Self::AccessDenied
            | Self::InvalidAccessKeyId
            | Self::SignatureDoesNotMatch
            | Self::RequestTimeTooSkewed => StatusCode::FORBIDDEN,
            Self::NoSuchBucket | Self::NoSuchKey => StatusCode::NOT_FOUND,
            Self::BucketAlreadyExists => StatusCode::CONFLICT,
            Self::BucketNotEmpty => StatusCode::CONFLICT,
            Self::InvalidRange => StatusCode::RANGE_NOT_SATISFIABLE,
            Self::NotImplemented => StatusCode::NOT_IMPLEMENTED,
            Self::InternalError => StatusCode::INTERNAL_SERVER_ERROR,
            Self::AuthorizationHeaderMalformed | Self::InvalidBucketName | Self::InvalidRequest => {
                StatusCode::BAD_REQUEST
            }
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
    use oes_auth::CredentialManager;
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
                None,
            )
            .await
            .expect("credential manager"),
        );
        let provider: Arc<dyn SigningCredentialProvider> = credentials.clone();
        (
            directory,
            router(S3State::new(services, provider)),
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

        let unsupported_copy = application
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
            .expect("unsupported copy response");
        assert_eq!(unsupported_copy.status(), StatusCode::NOT_IMPLEMENTED);
        assert_eq!(
            xml_value(&body_text(unsupported_copy).await, "Code"),
            Some("NotImplemented")
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

        let unsupported_multipart_delete = application
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
            .expect("unsupported multipart delete response");
        assert_eq!(
            unsupported_multipart_delete.status(),
            StatusCode::NOT_IMPLEMENTED
        );

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
