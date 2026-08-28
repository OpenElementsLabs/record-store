use std::{collections::BTreeMap, io};

use axum::{
    body::Body,
    extract::Extension,
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{self, HeaderName},
    },
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use futures_util::TryStreamExt;
use record_store_core::{BucketName, ByteRange, ObjectKey, ObjectMetadata, VersionId};
use record_store_service::ServiceGetResult;
use serde::Serialize;

use crate::error::{S3Error, S3ErrorKind};
use crate::handlers::listing::decode_query_component;
use crate::sigv4::S3RequestId;
use crate::*;

pub(crate) async fn unsupported_operation(
    Extension(request_id): Extension<S3RequestId>,
) -> S3Error {
    S3Error::new(S3ErrorKind::NotImplemented, request_id, "/")
}

pub(crate) fn streaming_response(result: ServiceGetResult) -> Result<Response, S3Error> {
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

pub(crate) fn conditional_streaming_response(
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
pub(crate) enum ConditionalOutcome {
    Proceed,
    NotModified,
}

pub(crate) fn evaluate_conditions(
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

pub(crate) fn apply_object_headers(
    response: &mut Response,
    metadata: &ObjectMetadata,
    length: u64,
) {
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

pub(crate) fn insert_etag(response: &mut Response, metadata: &ObjectMetadata) {
    if let Ok(value) = HeaderValue::from_str(&format!("\"{}\"", metadata.etag)) {
        response.headers_mut().insert(header::ETAG, value);
    }
}

pub(crate) fn insert_version_id(response: &mut Response, version_id: VersionId) {
    if let Ok(value) = HeaderValue::from_str(&version_id.to_string()) {
        response
            .headers_mut()
            .insert(HeaderName::from_static("x-amz-version-id"), value);
    }
}

pub(crate) fn custom_metadata(
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

pub(crate) fn reject_subresources(
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

pub(crate) fn unsupported_put_headers(headers: &HeaderMap) -> bool {
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

pub(crate) fn parse_range(value: &str, size: u64) -> Result<ByteRange, S3ErrorKind> {
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

pub(crate) fn bucket_name(value: &str, request_id: &S3RequestId) -> Result<BucketName, S3Error> {
    BucketName::new(value)
        .map_err(|_| S3Error::new(S3ErrorKind::InvalidBucketName, request_id.clone(), value))
}

pub(crate) fn object_key(
    value: &str,
    request_id: &S3RequestId,
    resource: &str,
) -> Result<ObjectKey, S3Error> {
    ObjectKey::new(value)
        .map_err(|_| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), resource))
}

pub(crate) fn encode_continuation_token(value: &str) -> String {
    URL_SAFE_NO_PAD.encode(value.as_bytes())
}

pub(crate) fn decode_continuation_token(value: &str) -> Result<String, S3ErrorKind> {
    let decoded = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| S3ErrorKind::InvalidRequest)?;
    String::from_utf8(decoded).map_err(|_| S3ErrorKind::InvalidRequest)
}

pub(crate) fn insert_request_id(response: &mut Response, request_id: &S3RequestId) {
    if let Ok(value) = HeaderValue::from_str(&request_id.0) {
        response
            .headers_mut()
            .insert(REQUEST_ID_HEADER.clone(), value);
    }
}

pub(crate) fn xml_response<T: Serialize>(
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
