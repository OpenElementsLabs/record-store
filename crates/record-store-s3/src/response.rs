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
use record_store_core::{
    BucketName, ByteRange, ObjectKey, ObjectLockState, ObjectMetadata, Retention, VersionId,
};
use record_store_service::ServiceGetResult;
use serde::Serialize;

use crate::error::{S3Error, S3ErrorKind};
use crate::handlers::listing::decode_query_component;
use crate::sigv4::S3RequestId;
use crate::xml::{format_retain_until, legal_hold_status, parse_legal_hold, parse_retain_until};
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

/// The Object Lock request headers this adapter understands.
pub(crate) const OBJECT_LOCK_MODE: &str = "x-amz-object-lock-mode";
pub(crate) const OBJECT_LOCK_RETAIN_UNTIL: &str = "x-amz-object-lock-retain-until-date";
pub(crate) const OBJECT_LOCK_LEGAL_HOLD: &str = "x-amz-object-lock-legal-hold";
pub(crate) const BYPASS_GOVERNANCE: &str = "x-amz-bypass-governance-retention";
const SUPPORTED_OBJECT_LOCK_HEADERS: [&str; 3] = [
    OBJECT_LOCK_MODE,
    OBJECT_LOCK_RETAIN_UNTIL,
    OBJECT_LOCK_LEGAL_HOLD,
];

pub(crate) fn unsupported_put_headers(headers: &HeaderMap) -> bool {
    headers.contains_key("x-amz-copy-source") || unsupported_write_headers(headers)
}

/// A copy refuses what a PUT refuses, and every `x-amz-copy-source-*` header:
/// conditional copies and source encryption keys are not implemented, and a
/// copy that ignored its precondition would overwrite what the client meant to
/// protect.
pub(crate) fn unsupported_copy_headers(headers: &HeaderMap) -> bool {
    unsupported_write_headers(headers)
        || headers.contains_key("x-amz-tagging-directive")
        || headers
            .keys()
            .any(|name| name.as_str().starts_with("x-amz-copy-source-"))
}

fn unsupported_write_headers(headers: &HeaderMap) -> bool {
    const UNSUPPORTED: [&str; 4] = [
        "x-amz-acl",
        "x-amz-server-side-encryption",
        "x-amz-tagging",
        "x-amz-website-redirect-location",
    ];
    UNSUPPORTED.iter().any(|name| headers.contains_key(*name))
        // The three Object Lock headers below are honoured. Any other member of
        // that family is still an unimplemented semantic and is refused rather
        // than dropped, so a client never believes a lock it did not get.
        || headers.keys().any(|name| {
            let name = name.as_str();
            name.starts_with("x-amz-object-lock-")
                && !SUPPORTED_OBJECT_LOCK_HEADERS.contains(&name)
        })
        || headers
            .get("x-amz-storage-class")
            .is_some_and(|value| value != "STANDARD")
}

/// Reads the Object Lock a write request asks for.
///
/// `None` means the request said nothing, so the bucket default applies.
/// `Some` means the caller was explicit, including explicitly asking for no
/// retention with only a legal hold.
pub(crate) fn requested_object_lock(
    headers: &HeaderMap,
    request_id: &S3RequestId,
    resource: &str,
) -> Result<Option<ObjectLockState>, S3Error> {
    let invalid = || S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), resource);
    let text = |name: &str| -> Result<Option<&str>, S3Error> {
        headers
            .get(name)
            .map(|value| value.to_str().map_err(|_| invalid()))
            .transpose()
    };
    let mode = text(OBJECT_LOCK_MODE)?;
    let retain_until = text(OBJECT_LOCK_RETAIN_UNTIL)?;
    let legal_hold = text(OBJECT_LOCK_LEGAL_HOLD)?;
    if mode.is_none() && retain_until.is_none() && legal_hold.is_none() {
        return Ok(None);
    }
    // A mode without a date, or a date without a mode, describes no retention
    // period at all. Guessing either half would invent a promise the caller
    // never made.
    let retention = match (mode, retain_until) {
        (None, None) => None,
        (Some(mode), Some(retain_until)) => Some(Retention {
            mode: record_store_core::RetentionMode::parse(mode).map_err(|_| invalid())?,
            retain_until: parse_retain_until(retain_until).map_err(|_| invalid())?,
        }),
        _ => return Err(invalid()),
    };
    let legal_hold = legal_hold
        .map(|value| parse_legal_hold(value).map_err(|_| invalid()))
        .transpose()?
        .unwrap_or(false);
    Ok(Some(ObjectLockState {
        retention,
        legal_hold,
    }))
}

/// Reads an authorized governance bypass from a request.
///
/// Presenting this header requires `s3:BypassGovernanceRetention`, which the
/// authorization middleware has already enforced by the time a handler asks.
pub(crate) fn requested_governance_bypass(headers: &HeaderMap) -> bool {
    headers
        .get(BYPASS_GOVERNANCE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("true"))
}

/// Adds the Object Lock response headers a read reports.
///
/// Absent headers mean an unlocked version, which is what S3 does: a client
/// reads the absence rather than an explicit "none".
pub(crate) fn apply_object_lock_headers(response: &mut Response, state: &ObjectLockState) {
    if let Some(retention) = state.retention {
        if let Ok(value) = HeaderValue::from_str(retention.mode.as_str()) {
            response
                .headers_mut()
                .insert(HeaderName::from_static(OBJECT_LOCK_MODE), value);
        }
        if let Ok(value) = HeaderValue::from_str(&format_retain_until(retention.retain_until)) {
            response
                .headers_mut()
                .insert(HeaderName::from_static(OBJECT_LOCK_RETAIN_UNTIL), value);
        }
    }
    if state.legal_hold {
        response.headers_mut().insert(
            HeaderName::from_static(OBJECT_LOCK_LEGAL_HOLD),
            HeaderValue::from_static(legal_hold_status(true)),
        );
    }
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
    // A start at or past the end is unsatisfiable. Catching it here is what
    // turns it into a 416 for the client; left to the storage layer it arrives
    // as an internal error, which tells a client to retry something that can
    // never succeed.
    if start >= size {
        return Err(S3ErrorKind::InvalidRange);
    }
    let length = if end.is_empty() {
        size.checked_sub(start).ok_or(S3ErrorKind::InvalidRange)?
    } else {
        let end: u64 = end.parse().map_err(|_| S3ErrorKind::InvalidRange)?;
        if end < start {
            return Err(S3ErrorKind::InvalidRange);
        }
        let requested = end
            .checked_sub(start)
            .and_then(|value| value.checked_add(1))
            .ok_or(S3ErrorKind::InvalidRange)?;
        // RFC 7233: a last-byte-pos at or past the end of the representation
        // means "to the end", not an error. The suffix branch above already
        // clamps; without the same clamp here, `bytes=0-<huge>` returned a
        // length reaching past EOF and stayed correct only because every
        // caller happens to pass it through `ByteRange::resolve`, which
        // truncates. Clamping at the source makes the returned range valid on
        // its own rather than by the grace of a later call. `start < size` is
        // established above, so the subtraction cannot wrap and the clamped
        // length cannot reach zero.
        requested.min(size - start)
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
