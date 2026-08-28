use axum::{
    extract::{Extension, Request, State},
    http::{
        HeaderMap, HeaderValue, Method, StatusCode, Uri,
        header::{self, HeaderName},
    },
    response::{IntoResponse, Response},
};
use percent_encoding::percent_decode_str;
use record_store_core::{BucketName, CorsGrant, CorsMethod, parse_requested_headers};

use crate::error::{S3Error, S3ErrorKind, service_error};
use crate::response::bucket_name;
use crate::sigv4::S3RequestId;
use crate::*;

pub(crate) fn is_cors_preflight(method: &Method, headers: &HeaderMap) -> bool {
    method == Method::OPTIONS
        && headers.contains_key(header::ORIGIN)
        && headers.contains_key(header::ACCESS_CONTROL_REQUEST_METHOD)
}

pub(crate) async fn cors_grant_for_request(
    state: &S3State,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    request_id: &S3RequestId,
) -> Option<CorsGrant> {
    let origin = headers.get(header::ORIGIN)?.to_str().ok()?;
    let method = CorsMethod::parse(method.as_str()).ok()?;
    let name = bucket_name_from_uri(uri, request_id).ok()?;
    let bucket = state.services.buckets.head(&name).await.ok()?;
    let configuration = bucket.cors.as_ref()?;
    let rule = configuration.match_request(origin, method)?;
    Some(CorsGrant::response(rule, origin))
}

pub(crate) async fn cors_preflight(
    State(state): State<S3State>,
    Extension(request_id): Extension<S3RequestId>,
    request: Request,
) -> Result<Response, S3Error> {
    let resource = request.uri().path();
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), resource))?;
    let requested_method = request
        .headers()
        .get(header::ACCESS_CONTROL_REQUEST_METHOD)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| CorsMethod::parse(value).ok())
        .ok_or_else(|| S3Error::new(S3ErrorKind::AccessDenied, request_id.clone(), resource))?;
    let requested_headers = request
        .headers()
        .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
        .and_then(|value| value.to_str().ok())
        .map_or_else(Vec::new, parse_requested_headers);
    if requested_headers
        .iter()
        .any(|name| HeaderName::from_bytes(name.as_bytes()).is_err())
    {
        return Err(S3Error::new(
            S3ErrorKind::AccessDenied,
            request_id,
            resource,
        ));
    }
    let name = bucket_name_from_uri(request.uri(), &request_id)?;
    let bucket = state
        .services
        .buckets
        .head(&name)
        .await
        .map_err(|error| service_error(error, request_id.clone(), resource))?;
    let configuration = bucket
        .cors
        .as_ref()
        .ok_or_else(|| S3Error::new(S3ErrorKind::AccessDenied, request_id.clone(), resource))?;
    let rule = configuration
        .match_preflight(origin, requested_method, &requested_headers)
        .ok_or_else(|| S3Error::new(S3ErrorKind::AccessDenied, request_id.clone(), resource))?;
    let grant = CorsGrant::preflight(rule, origin, &requested_headers);
    let mut response = StatusCode::OK.into_response();
    apply_cors_grant(&mut response, &grant, true);
    Ok(response)
}

pub(crate) fn bucket_name_from_uri(
    uri: &Uri,
    request_id: &S3RequestId,
) -> Result<BucketName, S3Error> {
    let segment = uri
        .path()
        .trim_start_matches('/')
        .split('/')
        .next()
        .filter(|segment| !segment.is_empty())
        .ok_or_else(|| {
            S3Error::new(
                S3ErrorKind::InvalidBucketName,
                request_id.clone(),
                uri.path(),
            )
        })?;
    let decoded = String::from_utf8(percent_decode_str(segment).collect()).map_err(|_| {
        S3Error::new(
            S3ErrorKind::InvalidBucketName,
            request_id.clone(),
            uri.path(),
        )
    })?;
    bucket_name(&decoded, request_id)
}

pub(crate) fn apply_cors_grant(response: &mut Response, grant: &CorsGrant, preflight: bool) {
    insert_cors_header(
        response,
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        &grant.allow_origin,
    );
    if let Some(methods) = grant.allow_methods.as_deref() {
        insert_cors_header(response, header::ACCESS_CONTROL_ALLOW_METHODS, methods);
    }
    if let Some(headers) = grant.allow_headers.as_deref() {
        insert_cors_header(response, header::ACCESS_CONTROL_ALLOW_HEADERS, headers);
    }
    if let Some(headers) = grant.expose_headers.as_deref() {
        insert_cors_header(response, header::ACCESS_CONTROL_EXPOSE_HEADERS, headers);
    }
    if let Some(seconds) = grant.max_age_seconds {
        insert_cors_header(
            response,
            header::ACCESS_CONTROL_MAX_AGE,
            &seconds.to_string(),
        );
    }
    let vary = match (preflight, grant.is_wildcard()) {
        (true, true) => "Access-Control-Request-Method, Access-Control-Request-Headers",
        (true, false) => "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
        (false, false) => "Origin",
        (false, true) => return,
    };
    response
        .headers_mut()
        .append(header::VARY, HeaderValue::from_static(vary));
}

pub(crate) fn insert_cors_header(response: &mut Response, name: HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        response.headers_mut().insert(name, value);
    }
}
