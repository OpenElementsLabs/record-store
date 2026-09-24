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
use record_store_audit::{
    AuditEvent, AuditResult,
    intent::{intent_event, method_mutates, outcome_event, result_for_status},
};
use record_store_auth::{
    Action, AuthorizationContext, CredentialLookupError, Permission, Principal,
};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tracing::info;

use crate::cors::{apply_cors_grant, cors_grant_for_request, is_cors_preflight};
use crate::error::{S3Error, S3ErrorKind};
use crate::handlers::listing::query_map;
use crate::response::{
    OBJECT_LOCK_LEGAL_HOLD, OBJECT_LOCK_MODE, OBJECT_LOCK_RETAIN_UNTIL, insert_request_id,
};
use crate::sigv4::{
    Authenticated, ParsedAuthorization, ParsedPresign, PayloadHash, S3RequestId,
    calculate_signature, canonical_request, parse_amz_date, parse_payload_hash, parse_request_time,
    reject_unsigned_amz_headers,
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
    // Resolved under the deployment's trusted-proxy policy rather than taken
    // from the socket: behind a proxy the socket is the proxy for every caller
    // in the world, and an unverified header is whatever the caller typed.
    let source_ip = state
        .trusted_proxies
        .client_address(
            request
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|connect| connect.0.ip()),
            request
                .headers()
                .get("x-forwarded-for")
                .and_then(|value| value.to_str().ok()),
        )
        .map(|address| address.to_string());
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
    let mut intent: Option<AuditEvent> = None;
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
                // The intent is written before the handler runs, so a crash or
                // an audit-store failure during the mutation cannot leave a
                // committed change with nothing in the trail naming it. A
                // request that cannot be announced is not performed.
                if method_mutates(method.as_str()) {
                    match write_mutation_intent(
                        &state,
                        &request_id,
                        &method,
                        &audit_resource,
                        Some(&authenticated.principal),
                        source_ip.clone(),
                    )
                    .await
                    {
                        Ok(written) => intent = written,
                        Err(()) => {
                            let mut response = S3Error::new(
                                S3ErrorKind::ServiceUnavailable,
                                request_id.clone(),
                                &audit_resource,
                            )
                            .into_response();
                            insert_request_id(&mut response, &request_id);
                            return response;
                        }
                    }
                }
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
    match &intent {
        // A mutation announced itself before it ran, so its completion is
        // written as the second half of that pair rather than as an unrelated
        // record: they carry the same principal, resource, and request.
        Some(intent) => {
            if let Some(audit) = &state.audit {
                let outcome = outcome_event(intent, result_for_status(response.status().as_u16()));
                if let Err(error) = audit.append(&outcome).await {
                    // The intent is already durable, so the operation is not
                    // lost — it is left visibly unresolved, which is the
                    // honest state and the one an operator can act on.
                    tracing::error!(
                        %error,
                        request_id = %request_id.0,
                        intent_event_id = %intent.event_id,
                        "durable S3 audit outcome append failed; the intent record stands alone"
                    );
                }
            }
        }
        None => {
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
        }
    }
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

/// Writes the intent record for a mutating request.
///
/// Returns `Err(())` when the trail cannot be written. A deployment that
/// audits is a deployment where an unrecordable mutation is refused: the
/// alternative is a change nobody can account for afterwards, which is worse
/// than a request that fails and can be retried.
async fn write_mutation_intent(
    state: &S3State,
    request_id: &S3RequestId,
    method: &Method,
    resource: &str,
    principal: Option<&Principal>,
    source_ip: Option<String>,
) -> Result<Option<AuditEvent>, ()> {
    let Some(audit) = &state.audit else {
        return Ok(None);
    };
    let credential_id = principal.and_then(|principal| match principal {
        Principal::ServiceAccount { credential_id, .. } => *credential_id,
        Principal::System { .. } | Principal::Anonymous => None,
    });
    let event = intent_event(
        Some(request_id.0.clone()),
        principal_name(principal),
        credential_id,
        source_ip,
        format!("s3:{}", method.as_str()),
        resource.to_owned(),
    );
    match audit.append(&event).await {
        Ok(()) => Ok(Some(event)),
        Err(error) => {
            tracing::error!(
                %error,
                request_id = %request_id.0,
                "refusing a mutating S3 request: its audit intent could not be made durable"
            );
            Err(())
        }
    }
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
    let credential_id = principal.and_then(|principal| match principal {
        Principal::ServiceAccount { credential_id, .. } => *credential_id,
        Principal::System { .. } | Principal::Anonymous => None,
    });
    let principal = principal_name(principal);
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

/// Renders a principal the way every durable record names it.
///
/// Audit records and Object Lock bypass records have to agree on this: an
/// operator correlating the two would otherwise be matching two spellings of
/// the same caller. It never contains credential material.
pub(crate) fn principal_name(principal: Option<&Principal>) -> String {
    match principal {
        Some(Principal::ServiceAccount { id, .. }) => format!("service_account:{id}"),
        Some(Principal::System { component }) => format!("system:{component}"),
        Some(Principal::Anonymous) | None => "anonymous".to_owned(),
    }
}

pub(crate) async fn verify_request(
    state: &S3State,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Authenticated, S3ErrorKind> {
    crate::sigv4::reject_streaming_payload(&headers)?;
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
    // Checked before the credential lookup, so a request that could be
    // carrying terms its signer never approved never reaches the secret store.
    reject_unsigned_amz_headers(&headers, &parsed.signed_headers)?;
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
    let reading = request.method() == Method::GET || request.method() == Method::HEAD;
    let action = if key.is_none() {
        // Bucket Object Lock configuration sits with versioning and CORS under
        // the one coarse bucket-administration permission, because the three
        // are the same kind of decision about the same object.
        if reading
            && !query.contains_key("versioning")
            && !query.contains_key("cors")
            && !query.contains_key("object-lock")
        {
            Action::ListBucket
        } else {
            Action::ManageBucket
        }
    } else if query.contains_key("retention") {
        if reading {
            Action::GetObjectRetention
        } else {
            Action::PutObjectRetention
        }
    } else if query.contains_key("legal-hold") {
        if reading {
            Action::GetObjectLegalHold
        } else {
            Action::PutObjectLegalHold
        }
    } else if reading {
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
    let mut permissions = vec![Permission {
        action,
        resource: resource.clone(),
    }];
    // Asking to override a governance retention is its own permission on top of
    // whatever the request was already going to do. Requiring it here means a
    // handler can treat the header's presence as an authorized bypass, because
    // an unauthorized caller is refused before any handler runs.
    if key.is_some()
        && request
            .headers()
            .get("x-amz-bypass-governance-retention")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.eq_ignore_ascii_case("true"))
    {
        permissions.push(Permission {
            action: Action::BypassGovernanceRetention,
            resource: resource.clone(),
        });
    }
    // Setting a lock while writing is the same decision as setting it later
    // through ?retention or ?legal-hold, and harder to take back: a COMPLIANCE
    // retention written with the object outlives every credential that could
    // have removed it. So it needs the permission those subresources need, on
    // top of PutObject. This covers a plain PUT, a copy, and a multipart
    // initiation, which is where a multipart upload's lock is fixed. A bucket's
    // default retention is applied by the service when a request names no
    // lock, so it is never asked for here and needs nothing extra. Presence is
    // what counts, not the value: even `legal-hold: OFF` is an explicit lock
    // choice that takes the place of the bucket default.
    if key.is_some() {
        let headers = request.headers();
        if headers.contains_key(OBJECT_LOCK_MODE) || headers.contains_key(OBJECT_LOCK_RETAIN_UNTIL)
        {
            permissions.push(Permission {
                action: Action::PutObjectRetention,
                resource: resource.clone(),
            });
        }
        if headers.contains_key(OBJECT_LOCK_LEGAL_HOLD) {
            permissions.push(Permission {
                action: Action::PutObjectLegalHold,
                resource,
            });
        }
    }
    if let Some(source) = request
        .headers()
        .get("x-amz-copy-source")
        .and_then(|value| value.to_str().ok())
    {
        let source = String::from_utf8(percent_decode_str(source).collect())
            .map_err(|_| S3ErrorKind::InvalidRequest)?;
        let (source, source_query) = source
            .trim_start_matches('/')
            .split_once('?')
            .unwrap_or((source.trim_start_matches('/'), ""));
        // Naming a version reads that version, which may be one the current
        // object has since replaced or deleted. A GET with ?versionId needs
        // GetObjectVersion, and a copy is a read of the same kind.
        let action = if query_map(Some(source_query))?.contains_key("versionId") {
            Action::GetObjectVersion
        } else {
            Action::GetObject
        };
        permissions.push(Permission {
            action,
            resource: format!("bucket:{source}"),
        });
    }
    Ok(permissions)
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderValue, Method, StatusCode, header};
    use tower::ServiceExt;

    use crate::test_support::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use axum::response::Response;
    use chrono::{Duration, Utc};
    use record_store_auth::{
        Action, CredentialManager, IssuedServiceAccount, PolicyEffect, PolicyStatement,
    };
    use record_store_core::OrganizationId;

    #[tokio::test]
    async fn unsigned_puts_verify_signatures_and_checksums() {
        let (_directory, application, _) = test_router().await;
        let unsigned = [("x-amz-content-sha256", "UNSIGNED-PAYLOAD")];
        for (path, body) in [
            ("/unsigned-bucket", b"".as_slice()),
            ("/unsigned-bucket/key", b"hello".as_slice()),
        ] {
            let response = send(&application, Method::PUT, path, body, &unsigned).await;
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "{}",
                body_text(response).await
            );
        }
        let response = send(&application, Method::GET, "/unsigned-bucket/key", b"", &[]).await;
        assert_eq!(body_text(response).await, "hello");
        let invalid = signed_request(
            Method::PUT,
            "/unsigned-bucket/key",
            b"changed",
            &unsigned,
            TEST_ACCESS_KEY,
            "wrong-secret-at-least-sixteen",
            Utc::now(),
        );
        let response = application
            .clone()
            .oneshot(invalid)
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            xml_value(&body_text(response).await, "Code"),
            Some("SignatureDoesNotMatch")
        );
        let response = send(
            &application,
            Method::PUT,
            "/unsigned-bucket/key",
            b"changed",
            &[
                ("x-amz-content-sha256", "UNSIGNED-PAYLOAD"),
                (
                    "x-amz-checksum-sha256",
                    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
                ),
            ],
        )
        .await;
        assert_eq!(
            xml_value(&body_text(response).await, "Code"),
            Some("BadDigest")
        );
        let response = send(&application, Method::GET, "/unsigned-bucket/key", b"", &[]).await;
        assert_eq!(body_text(response).await, "hello");
    }

    #[tokio::test]
    async fn streaming_payloads_and_malformed_hashes_have_specific_errors() {
        let (_directory, application, _) = test_router().await;
        for headers in [
            vec![(
                "x-amz-content-sha256",
                "STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER",
            )],
            vec![("x-amz-content-sha256", "STREAMING-UNSIGNED-PAYLOAD-TRAILER")],
            vec![("x-amz-content-sha256", "STREAMING-AWS4-HMAC-SHA256-PAYLOAD")],
            vec![("content-encoding", "gzip, AWS-CHUNKED")],
            vec![("x-amz-trailer", "x-amz-checksum-crc32")],
        ] {
            let response = send(
                &application,
                Method::PUT,
                "/missing/key",
                b"framed",
                &headers,
            )
            .await;
            assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
            assert!(response.headers().contains_key("x-amz-request-id"));
            let body = body_text(response).await;
            assert_eq!(xml_value(&body, "Code"), Some("NotImplemented"));
            assert!(
                xml_value(&body, "Message")
                    .unwrap()
                    .contains("disable chunked encoding")
            );
        }
        let mut presigned = presigned_request(
            Method::PUT,
            "/missing/key",
            TEST_ACCESS_KEY,
            TEST_SECRET_KEY,
            Utc::now(),
            60,
        );
        presigned
            .headers_mut()
            .insert("content-encoding", HeaderValue::from_static("aws-chunked"));
        let response = application
            .clone()
            .oneshot(presigned)
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let response = send(
            &application,
            Method::PUT,
            "/missing/key",
            b"",
            &[("x-amz-content-sha256", "invalid")],
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_text(response).await;
        assert_eq!(xml_value(&body, "Code"), Some("InvalidRequest"));
        assert!(
            xml_value(&body, "Message")
                .unwrap()
                .contains("x-amz-content-sha256")
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

    /// A governance bypass is a permission, not a header. A caller who may
    /// delete versions but was never granted the bypass must not be able to
    /// overrule a retention simply by asking.
    #[tokio::test]
    async fn presenting_a_governance_bypass_requires_the_bypass_permission() {
        let (_directory, application, credentials) = test_router().await;
        make_locked_bucket(&application, "records").await;
        let until =
            (Utc::now() + Duration::days(30)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let version = put_returning_version(
            &application,
            "records",
            "draft.txt",
            b"hello",
            &[
                ("x-amz-object-lock-mode", "GOVERNANCE"),
                ("x-amz-object-lock-retain-until-date", &until),
            ],
        )
        .await;

        // Everything an ordinary writer needs, deliberately without the bypass.
        let ordinary_actions = vec![
            Action::ListBucket,
            Action::GetObject,
            Action::PutObject,
            Action::DeleteObject,
            Action::GetObjectVersion,
            Action::DeleteObjectVersion,
            Action::ManageBucket,
            Action::GetObjectRetention,
            Action::PutObjectRetention,
        ];
        let issued = credentials
            .create_service_account("lock-test-client", OrganizationId::new())
            .await
            .expect("issue service account");
        let policy = credentials
            .create_policy(
                "lock-test-no-bypass",
                "delete versions, but never overrule a retention",
                vec![PolicyStatement {
                    effect: PolicyEffect::Allow,
                    actions: ordinary_actions.clone(),
                    resources: vec!["bucket:*".into()],
                }],
            )
            .await
            .expect("create policy");
        credentials
            .attach_policy(issued.info.account.id, policy.id)
            .await
            .expect("attach policy");
        let secret = std::str::from_utf8(issued.secret.expose())
            .expect("secret text")
            .to_owned();

        let without_permission = application
            .clone()
            .oneshot(signed_request(
                Method::DELETE,
                &format!("/records/draft.txt?versionId={version}"),
                b"",
                &[("x-amz-bypass-governance-retention", "true")],
                &issued.info.credential.key_id,
                &secret,
                Utc::now(),
            ))
            .await
            .expect("bypass without permission");
        assert_eq!(
            without_permission.status(),
            StatusCode::FORBIDDEN,
            "the bypass header is refused before any handler runs"
        );

        // The same caller, once granted the bypass action, succeeds.
        let mut with_bypass = ordinary_actions;
        with_bypass.push(Action::BypassGovernanceRetention);
        let elevated = credentials
            .create_policy(
                "lock-test-with-bypass",
                "may overrule a governance retention",
                vec![PolicyStatement {
                    effect: PolicyEffect::Allow,
                    actions: with_bypass,
                    resources: vec!["bucket:*".into()],
                }],
            )
            .await
            .expect("create elevated policy");
        credentials
            .attach_policy(issued.info.account.id, elevated.id)
            .await
            .expect("attach elevated policy");

        let permitted = application
            .oneshot(signed_request(
                Method::DELETE,
                &format!("/records/draft.txt?versionId={version}"),
                b"",
                &[("x-amz-bypass-governance-retention", "true")],
                &issued.info.credential.key_id,
                &secret,
                Utc::now(),
            ))
            .await
            .expect("bypass with permission");
        assert_eq!(permitted.status(), StatusCode::NO_CONTENT);
    }

    /// Issues a service account allowed exactly `actions` on every bucket.
    async fn account_allowed(
        credentials: &CredentialManager,
        name: &str,
        actions: Vec<Action>,
    ) -> (IssuedServiceAccount, String) {
        let issued = credentials
            .create_service_account(name, OrganizationId::new())
            .await
            .expect("issue service account");
        grant(credentials, &issued, name, actions).await;
        let secret = std::str::from_utf8(issued.secret.expose())
            .expect("secret text")
            .to_owned();
        (issued, secret)
    }

    /// Attaches one more policy, allowing `actions` on every bucket.
    async fn grant(
        credentials: &CredentialManager,
        issued: &IssuedServiceAccount,
        name: &str,
        actions: Vec<Action>,
    ) {
        let policy = credentials
            .create_policy(
                name,
                "test-only grant",
                vec![PolicyStatement {
                    effect: PolicyEffect::Allow,
                    actions,
                    resources: vec!["bucket:*".into()],
                }],
            )
            .await
            .expect("create policy");
        credentials
            .attach_policy(issued.info.account.id, policy.id)
            .await
            .expect("attach policy");
    }

    /// Sends a request signed by a service account rather than root, which
    /// is never subject to policy.
    async fn send_as(
        application: &Router,
        (issued, secret): &(IssuedServiceAccount, String),
        method: Method,
        uri: &str,
        payload: &[u8],
        headers: &[(&str, &str)],
    ) -> Response {
        application
            .clone()
            .oneshot(signed_request(
                method,
                uri,
                payload,
                headers,
                &issued.info.credential.key_id,
                secret,
                Utc::now(),
            ))
            .await
            .expect("router responds")
    }

    async fn assert_refused_as_unsigned(response: Response) {
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = body_text(response).await;
        assert_eq!(xml_value(&body, "Code"), Some("AccessDenied"), "{body}");
        assert_eq!(
            xml_value(&body, "Message"),
            Some("There were headers present in the request which were not signed"),
            "{body}"
        );
    }

    fn retain_until(days: i64) -> String {
        (Utc::now() + Duration::days(days)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    /// A presigned URL grants what its signer signed and nothing more. A
    /// holder who could add an Object Lock header would write a COMPLIANCE
    /// retention nobody can remove, under the signer's name.
    #[tokio::test]
    async fn a_presigned_put_refuses_an_unsigned_lock_header_before_storing_anything() {
        let (_directory, application, _credentials) = test_router().await;
        make_locked_bucket(&application, "records").await;

        let mut put = presigned_request(
            Method::PUT,
            "/records/statement.pdf",
            TEST_ACCESS_KEY,
            TEST_SECRET_KEY,
            Utc::now(),
            60,
        );
        put.headers_mut().insert(
            "x-amz-object-lock-mode",
            HeaderValue::from_static("COMPLIANCE"),
        );
        put.headers_mut().insert(
            "x-amz-object-lock-retain-until-date",
            HeaderValue::from_static("2100-01-01T00:00:00.000Z"),
        );
        *put.body_mut() = Body::from("forged");
        let response = application
            .clone()
            .oneshot(put)
            .await
            .expect("presigned put");
        assert_refused_as_unsigned(response).await;

        let stored = send(
            &application,
            Method::GET,
            "/records/statement.pdf",
            b"",
            &[],
        )
        .await;
        assert_eq!(
            stored.status(),
            StatusCode::NOT_FOUND,
            "nothing reached storage"
        );

        // Refused before the credential is looked up: an unknown key is not
        // reported as unknown, because the store was never asked.
        let mut unknown = presigned_request(
            Method::PUT,
            "/records/statement.pdf",
            "unknown-access",
            TEST_SECRET_KEY,
            Utc::now(),
            60,
        );
        unknown.headers_mut().insert(
            "x-amz-object-lock-legal-hold",
            HeaderValue::from_static("ON"),
        );
        let response = application
            .clone()
            .oneshot(unknown)
            .await
            .expect("presigned put");
        assert_refused_as_unsigned(response).await;
    }

    /// A copy source turns an upload into a read of anything the signer can
    /// read, so a presigned PUT must not accept one its signer did not sign.
    #[tokio::test]
    async fn a_presigned_put_refuses_an_unsigned_copy_source() {
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;
        put(&application, "photos", "private.txt", b"private").await;

        let mut upload = presigned_request(
            Method::PUT,
            "/photos/public.txt",
            TEST_ACCESS_KEY,
            TEST_SECRET_KEY,
            Utc::now(),
            60,
        );
        upload.headers_mut().insert(
            "x-amz-copy-source",
            HeaderValue::from_static("/photos/private.txt"),
        );
        let response = application
            .clone()
            .oneshot(upload)
            .await
            .expect("presigned put");
        assert_refused_as_unsigned(response).await;

        let copied = send(&application, Method::GET, "/photos/public.txt", b"", &[]).await;
        assert_eq!(copied.status(), StatusCode::NOT_FOUND, "nothing was copied");
    }

    /// Header authentication is held to the same rule: a signature covers
    /// the headers it names, and any other `x-amz-*` header is refused rather
    /// than acted on.
    #[tokio::test]
    async fn a_header_signed_request_refuses_an_unsigned_amz_header() {
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;
        let metadata = [("x-amz-meta-foo", "bar")];

        let unsigned = application
            .clone()
            .oneshot(signed_request_leaving_unsigned(
                Method::PUT,
                "/photos/a.txt",
                b"hello",
                &metadata,
                &["x-amz-meta-foo"],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                Utc::now(),
            ))
            .await
            .expect("unsigned metadata");
        assert_refused_as_unsigned(unsigned).await;
        let stored = send(&application, Method::GET, "/photos/a.txt", b"", &[]).await;
        assert_eq!(stored.status(), StatusCode::NOT_FOUND);

        let signed = send(
            &application,
            Method::PUT,
            "/photos/a.txt",
            b"hello",
            &metadata,
        )
        .await;
        assert_eq!(signed.status(), StatusCode::OK);
        let head = send(&application, Method::HEAD, "/photos/a.txt", b"", &[]).await;
        assert_eq!(
            response_header(&head, "x-amz-meta-foo").as_deref(),
            Some("bar")
        );
    }

    /// SigV4 header authentication requires the payload hash and the request
    /// time to be signed. Unsigned, the hash could be swapped for
    /// UNSIGNED-PAYLOAD and the time replayed.
    #[tokio::test]
    async fn the_payload_hash_and_request_time_must_be_signed() {
        let (_directory, application, _credentials) = test_router().await;
        for header in ["x-amz-content-sha256", "x-amz-date"] {
            let response = application
                .clone()
                .oneshot(signed_request_leaving_unsigned(
                    Method::GET,
                    "/",
                    b"",
                    &[],
                    &[header],
                    TEST_ACCESS_KEY,
                    TEST_SECRET_KEY,
                    Utc::now(),
                ))
                .await
                .expect("response");
            assert_refused_as_unsigned(response).await;
        }
    }

    /// Everything an ordinary writer needs, with no Object Lock permission.
    fn writer_actions() -> Vec<Action> {
        vec![Action::ListBucket, Action::GetObject, Action::PutObject]
    }

    /// Writing an object with a retention is the same decision as setting one
    /// through ?retention afterwards, and needs the same permission. A PUT and
    /// a multipart initiation, where a multipart upload's lock is fixed, are
    /// held to it alike.
    #[tokio::test]
    async fn a_retention_written_with_the_object_requires_the_retention_permission() {
        let (_directory, application, credentials) = test_router().await;
        make_locked_bucket(&application, "records").await;
        let writer = account_allowed(&credentials, "lock-writer", writer_actions()).await;
        let until = retain_until(30);
        let lock = [
            ("x-amz-object-lock-mode", "COMPLIANCE"),
            ("x-amz-object-lock-retain-until-date", until.as_str()),
        ];

        let refused = send_as(
            &application,
            &writer,
            Method::PUT,
            "/records/a.txt",
            b"hello",
            &lock,
        )
        .await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        let stored = send(&application, Method::GET, "/records/a.txt", b"", &[]).await;
        assert_eq!(
            stored.status(),
            StatusCode::NOT_FOUND,
            "nothing reached storage"
        );
        let initiation = send_as(
            &application,
            &writer,
            Method::POST,
            "/records/big.bin?uploads",
            b"",
            &lock,
        )
        .await;
        assert_eq!(initiation.status(), StatusCode::FORBIDDEN);

        grant(
            &credentials,
            &writer.0,
            "lock-writer-retention",
            vec![Action::PutObjectRetention],
        )
        .await;
        let permitted = send_as(
            &application,
            &writer,
            Method::PUT,
            "/records/a.txt",
            b"hello",
            &lock,
        )
        .await;
        assert_eq!(permitted.status(), StatusCode::OK);
        let head = send(&application, Method::HEAD, "/records/a.txt", b"", &[]).await;
        assert_eq!(
            response_header(&head, "x-amz-object-lock-mode").as_deref(),
            Some("COMPLIANCE")
        );
        let initiation = send_as(
            &application,
            &writer,
            Method::POST,
            "/records/big.bin?uploads",
            b"",
            &lock,
        )
        .await;
        assert_eq!(initiation.status(), StatusCode::OK);
    }

    /// The legal-hold header needs the legal-hold permission, whatever its
    /// value. Even `OFF` is an explicit lock choice that takes the place of
    /// the bucket default, so it is not a free way to decline one.
    #[tokio::test]
    async fn a_legal_hold_written_with_the_object_requires_the_legal_hold_permission() {
        let (_directory, application, credentials) = test_router().await;
        make_locked_bucket(&application, "records").await;
        let writer = account_allowed(&credentials, "hold-writer", writer_actions()).await;

        for value in ["ON", "OFF"] {
            let refused = send_as(
                &application,
                &writer,
                Method::PUT,
                "/records/held.txt",
                b"hello",
                &[("x-amz-object-lock-legal-hold", value)],
            )
            .await;
            assert_eq!(
                refused.status(),
                StatusCode::FORBIDDEN,
                "legal hold {value}"
            );
        }
        let stored = send(&application, Method::GET, "/records/held.txt", b"", &[]).await;
        assert_eq!(stored.status(), StatusCode::NOT_FOUND);

        grant(
            &credentials,
            &writer.0,
            "hold-writer-legal-hold",
            vec![Action::PutObjectLegalHold],
        )
        .await;
        let permitted = send_as(
            &application,
            &writer,
            Method::PUT,
            "/records/held.txt",
            b"hello",
            &[("x-amz-object-lock-legal-hold", "ON")],
        )
        .await;
        assert_eq!(permitted.status(), StatusCode::OK);
        let head = send(&application, Method::HEAD, "/records/held.txt", b"", &[]).await;
        assert_eq!(
            response_header(&head, "x-amz-object-lock-legal-hold").as_deref(),
            Some("ON")
        );
    }

    /// A bucket default is the bucket owner's decision, applied by the
    /// service. A writer who asks for no lock needs only PutObject, and still
    /// gets the default.
    #[tokio::test]
    async fn a_bucket_default_retention_applies_to_a_writer_holding_only_put_object() {
        let (_directory, application, credentials) = test_router().await;
        make_locked_bucket(&application, "records").await;
        let configured = send(
            &application,
            Method::PUT,
            "/records?object-lock",
            b"<ObjectLockConfiguration><ObjectLockEnabled>Enabled</ObjectLockEnabled><Rule><DefaultRetention><Mode>GOVERNANCE</Mode><Days>30</Days></DefaultRetention></Rule></ObjectLockConfiguration>",
            &[],
        )
        .await;
        assert_eq!(configured.status(), StatusCode::OK);
        let writer = account_allowed(&credentials, "default-writer", vec![Action::PutObject]).await;

        let written = send_as(
            &application,
            &writer,
            Method::PUT,
            "/records/a.txt",
            b"hello",
            &[],
        )
        .await;
        assert_eq!(written.status(), StatusCode::OK);
        let head = send(&application, Method::HEAD, "/records/a.txt", b"", &[]).await;
        assert_eq!(
            response_header(&head, "x-amz-object-lock-mode").as_deref(),
            Some("GOVERNANCE"),
            "the default still materializes onto the version"
        );
    }

    /// Copying a named version reads that version, which the current object
    /// may have replaced. That needs GetObjectVersion, as a GET with
    /// ?versionId does.
    #[tokio::test]
    async fn copying_a_named_version_requires_get_object_version() {
        let (_directory, application, credentials) = test_router().await;
        make_bucket(&application, "photos").await;
        let versioning = send(
            &application,
            Method::PUT,
            "/photos?versioning",
            b"<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>",
            &[],
        )
        .await;
        assert_eq!(versioning.status(), StatusCode::OK);
        let first = put_returning_version(&application, "photos", "a.txt", b"one", &[]).await;
        put(&application, "photos", "a.txt", b"two").await;
        let copier = account_allowed(&credentials, "copier", writer_actions()).await;
        let versioned_source = format!("/photos/a.txt?versionId={first}");

        let refused = send_as(
            &application,
            &copier,
            Method::PUT,
            "/photos/copy.txt",
            b"",
            &[("x-amz-copy-source", &versioned_source)],
        )
        .await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        let current = send_as(
            &application,
            &copier,
            Method::PUT,
            "/photos/copy.txt",
            b"",
            &[("x-amz-copy-source", "/photos/a.txt")],
        )
        .await;
        assert_eq!(
            current.status(),
            StatusCode::OK,
            "the current version needs only GetObject"
        );

        grant(
            &credentials,
            &copier.0,
            "copier-versions",
            vec![Action::GetObjectVersion],
        )
        .await;
        let permitted = send_as(
            &application,
            &copier,
            Method::PUT,
            "/photos/copy.txt",
            b"",
            &[("x-amz-copy-source", &versioned_source)],
        )
        .await;
        assert_eq!(permitted.status(), StatusCode::OK);
        let copied = send(&application, Method::GET, "/photos/copy.txt", b"", &[]).await;
        assert_eq!(body_text(copied).await, "one");
    }
}

#[cfg(test)]
mod audit_intent_tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering},
    };

    use async_trait::async_trait;
    use axum::http::{Method, StatusCode};
    use chrono::Utc;
    use record_store_audit::{
        AuditError, AuditEvent, AuditPage, AuditQuery, AuditRepository, AuditResult,
        ChainVerification, RedbAuditRepository, intent::INTENT_EVENT_ID,
    };
    use tower::ServiceExt;

    use crate::test_support::{
        TEST_ACCESS_KEY, TEST_SECRET_KEY, signed_request, test_router_with_audit,
    };

    /// An audit store that can be made to fail on demand.
    ///
    /// Injecting the failure is the only way to exercise what happens when the
    /// trail cannot be written, and that path is exactly the one a real
    /// deployment hits when the audit disk fills.
    struct FaultyAudit {
        inner: RedbAuditRepository,
        failing: AtomicBool,
        appends: AtomicUsize,
        /// Appends succeed until this many have been made, then fail.
        ///
        /// Lets a test place the failure *between* a mutation's two records,
        /// which is the window a crash would land in.
        fail_after: AtomicI64,
    }

    #[async_trait]
    impl AuditRepository for FaultyAudit {
        async fn append(&self, event: &AuditEvent) -> Result<(), AuditError> {
            let made = self.appends.fetch_add(1, Ordering::Relaxed);
            if self.failing.load(Ordering::Relaxed)
                || made as i64 >= self.fail_after.load(Ordering::Relaxed)
            {
                return Err(AuditError::Database {
                    operation: "append",
                    reason: "injected failure".into(),
                });
            }
            self.inner.append(event).await
        }

        async fn query(&self, query: AuditQuery) -> Result<AuditPage, AuditError> {
            self.inner.query(query).await
        }

        async fn verify_chain(
            &self,
            from_sequence: u64,
            limit: usize,
        ) -> Result<ChainVerification, AuditError> {
            self.inner.verify_chain(from_sequence, limit).await
        }

        async fn check_ready(&self) -> Result<(), AuditError> {
            self.inner.check_ready().await
        }
    }

    async fn faulty(directory: &std::path::Path) -> Arc<FaultyAudit> {
        Arc::new(FaultyAudit {
            inner: RedbAuditRepository::open(directory.join("audit.redb"))
                .await
                .expect("audit"),
            failing: AtomicBool::new(false),
            appends: AtomicUsize::new(0),
            fail_after: AtomicI64::new(i64::MAX),
        })
    }

    async fn all_events(audit: &FaultyAudit) -> Vec<AuditEvent> {
        audit
            .query(AuditQuery {
                limit: 1_000,
                ..AuditQuery::default()
            })
            .await
            .expect("query")
            .events
    }

    /// A mutation announces itself before it happens and reports its outcome
    /// afterwards. Both records name the same principal, resource, and request,
    /// so a reader can pair them without guessing.
    #[tokio::test]
    async fn a_mutating_request_writes_an_intent_before_its_outcome() {
        let staging = tempfile::tempdir().expect("temporary directory");
        let audit = faulty(staging.path()).await;
        let (_directory, application, _credentials) =
            test_router_with_audit(audit.clone() as Arc<dyn AuditRepository>).await;

        let response = application
            .clone()
            .oneshot(signed_request(
                Method::PUT,
                "/audited-bucket",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                Utc::now(),
            ))
            .await
            .expect("create bucket");
        assert_eq!(response.status(), StatusCode::OK);

        let events = all_events(&audit).await;
        let intent = events
            .iter()
            .find(|event| event.result == AuditResult::Attempted)
            .expect("the mutation must announce itself before it runs");
        let outcome = events
            .iter()
            .find(|event| event.result == AuditResult::Success)
            .expect("and report its outcome afterwards");

        assert_eq!(intent.operation, "s3:PUT");
        assert_eq!(intent.resource, "/audited-bucket");
        assert_eq!(outcome.request_id, intent.request_id);
        assert_eq!(outcome.resource, intent.resource);
        assert_eq!(outcome.principal, intent.principal);
        assert_eq!(
            outcome.metadata.get(INTENT_EVENT_ID),
            Some(&intent.event_id.to_string()),
            "the outcome must name the intent it completes"
        );
        assert!(
            intent.timestamp <= outcome.timestamp,
            "the intent is written first"
        );
    }

    /// Reads are the overwhelming majority of traffic and change nothing, so
    /// they keep their single record. Doubling them would cost an audit write
    /// per GET and buy nothing.
    #[tokio::test]
    async fn a_read_leaves_one_record_and_no_intent() {
        let staging = tempfile::tempdir().expect("temporary directory");
        let audit = faulty(staging.path()).await;
        let (_directory, application, _credentials) =
            test_router_with_audit(audit.clone() as Arc<dyn AuditRepository>).await;

        let response = application
            .clone()
            .oneshot(signed_request(
                Method::GET,
                "/",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                Utc::now(),
            ))
            .await
            .expect("list buckets");
        assert_eq!(response.status(), StatusCode::OK);

        let events = all_events(&audit).await;
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].result, AuditResult::Success);
    }

    /// The point of writing the intent first is that the mutation cannot
    /// outrun it. When the trail cannot be written the request is refused, and
    /// nothing is stored — an unrecordable change is worse than a failed one.
    #[tokio::test]
    async fn a_mutation_is_refused_when_its_intent_cannot_be_made_durable() {
        let staging = tempfile::tempdir().expect("temporary directory");
        let audit = faulty(staging.path()).await;
        let (_directory, application, _credentials) =
            test_router_with_audit(audit.clone() as Arc<dyn AuditRepository>).await;

        audit.failing.store(true, Ordering::Relaxed);
        let response = application
            .clone()
            .oneshot(signed_request(
                Method::PUT,
                "/unrecordable",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                Utc::now(),
            ))
            .await
            .expect("create bucket");
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "a mutation that cannot be announced must not be performed"
        );

        audit.failing.store(false, Ordering::Relaxed);
        let listing = application
            .clone()
            .oneshot(signed_request(
                Method::GET,
                "/",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                Utc::now(),
            ))
            .await
            .expect("list buckets");
        assert_eq!(listing.status(), StatusCode::OK);
        let body = crate::test_support::body_text(listing).await;
        assert!(
            !body.contains("unrecordable"),
            "the refused bucket must not exist: {body}"
        );
    }

    /// The crash this whole arrangement exists for: the change commits and the
    /// record of its outcome is lost. What must survive is the announcement,
    /// naming who did what — leaving an operation the server visibly cannot
    /// account for, rather than one nothing mentions at all.
    #[tokio::test]
    async fn a_lost_outcome_leaves_the_announcement_standing() {
        let staging = tempfile::tempdir().expect("temporary directory");
        let audit = faulty(staging.path()).await;
        let (_directory, application, _credentials) =
            test_router_with_audit(audit.clone() as Arc<dyn AuditRepository>).await;

        // The intent is written, the bucket is created, and the outcome append
        // is then made to fail — which is what a crash in that window, or a
        // disk that filled between the two, looks like from here.
        let before = audit.appends.load(Ordering::Relaxed);
        let response = application
            .clone()
            .oneshot(signed_request(
                Method::PUT,
                "/half-recorded",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                Utc::now(),
            ))
            .await
            .expect("create bucket");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            audit.appends.load(Ordering::Relaxed) - before,
            2,
            "one announcement and one outcome"
        );

        // Now the same operation with the outcome lost.
        audit.fail_after.store(1, Ordering::Relaxed);
        audit.appends.store(0, Ordering::Relaxed);
        let response = application
            .clone()
            .oneshot(signed_request(
                Method::PUT,
                "/outcome-lost",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                Utc::now(),
            ))
            .await
            .expect("create bucket");
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "the change itself succeeded; only its outcome record was lost"
        );

        audit.fail_after.store(i64::MAX, Ordering::Relaxed);
        let events = all_events(&audit).await;
        let dangling: Vec<_> = events
            .iter()
            .filter(|event| {
                event.result == AuditResult::Attempted && event.resource == "/outcome-lost"
            })
            .collect();
        assert_eq!(
            dangling.len(),
            1,
            "the announcement must survive the lost outcome: {events:?}"
        );
        assert!(
            !events.iter().any(|event| {
                event.resource == "/outcome-lost" && event.result != AuditResult::Attempted
            }),
            "and there is deliberately no invented outcome: {events:?}"
        );
        // The dangling record is findable, which is what makes the gap
        // actionable rather than merely honest.
        let attempted = audit
            .query(AuditQuery {
                result: Some(AuditResult::Attempted),
                limit: 100,
                ..AuditQuery::default()
            })
            .await
            .expect("query")
            .events;
        assert!(
            attempted
                .iter()
                .any(|event| event.resource == "/outcome-lost")
        );
    }

    /// A refused request changes nothing, so it writes the single record it
    /// always has rather than announcing a mutation that never began.
    #[tokio::test]
    async fn a_denied_mutation_writes_no_intent() {
        let staging = tempfile::tempdir().expect("temporary directory");
        let audit = faulty(staging.path()).await;
        let (_directory, application, _credentials) =
            test_router_with_audit(audit.clone() as Arc<dyn AuditRepository>).await;

        let response = application
            .clone()
            .oneshot(signed_request(
                Method::PUT,
                "/denied-bucket",
                b"",
                &[],
                TEST_ACCESS_KEY,
                "the-wrong-secret-key-entirely-here",
                Utc::now(),
            ))
            .await
            .expect("create bucket");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let events = all_events(&audit).await;
        assert!(
            events
                .iter()
                .all(|event| event.result != AuditResult::Attempted),
            "authorization failed, so nothing was ever attempted: {events:?}"
        );
        assert_eq!(events.len(), 1, "{events:?}");
    }
}
