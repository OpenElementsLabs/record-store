use std::{collections::BTreeMap, net::SocketAddr, time::Instant};

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{
        HeaderMap, Method, StatusCode, Uri,
        header::{self},
    },
    middleware::Next,
    response::{IntoResponse, Response},
};
use chrono::Utc;
use percent_encoding::percent_decode_str;
use record_store_audit::{AuditEvent, AuditResult};
use record_store_auth::{
    Action, AuthorizationContext, CredentialLookupError, Permission, Principal,
};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tracing::info;

use crate::cors::{apply_cors_grant, cors_grant_for_request, is_cors_preflight};
use crate::error::{S3Error, S3ErrorKind};
use crate::handlers::listing::query_map;
use crate::response::insert_request_id;
use crate::sigv4::{
    Authenticated, ParsedAuthorization, ParsedPresign, PayloadHash, S3RequestId,
    calculate_signature, canonical_request, parse_amz_date, parse_payload_hash, parse_request_time,
};
use crate::*;

pub(crate) async fn authenticate_request(
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
    let is_preflight = is_cors_preflight(&method, &headers);
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
    let cors_grant = if is_preflight {
        None
    } else {
        cors_grant_for_request(&state, &method, &uri, &headers, &request_id).await
    };
    let mut audit_principal = None;
    let mut response = if is_preflight {
        next.run(request).await
    } else {
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
        match authorization {
            Ok(authenticated) => {
                audit_principal = Some(authenticated.principal.clone());
                request.extensions_mut().insert(authenticated.principal);
                request.extensions_mut().insert(authenticated.payload);
                next.run(request).await
            }
            Err(kind) => {
                S3Error::new(kind, request_id.clone(), request.uri().path()).into_response()
            }
        }
    };
    insert_request_id(&mut response, &request_id);
    if let Some(grant) = &cors_grant {
        apply_cors_grant(&mut response, grant, false);
    }
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

pub(crate) async fn append_s3_audit(
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
        event_id: record_store_core::AuditEventId::new(),
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

pub(crate) async fn verify_request(
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

pub(crate) async fn authorize_permissions(
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

pub(crate) fn request_permissions(request: &Request) -> Result<Vec<Permission>, S3ErrorKind> {
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
        if request.method() == Method::GET
            && !query.contains_key("versioning")
            && !query.contains_key("cors")
        {
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
