//! Management and public HTTP surfaces for share and embed capabilities.
//!
//! Two surfaces live here and they are deliberately kept apart. The management
//! routes sit under `/api/v1`, behind the same bearer authentication as every
//! other administrative operation, and are where capabilities are created,
//! inspected, and withdrawn. The public routes — `/s/{token}` and `/e/{token}` —
//! carry no session at all: the token in the path *is* the authorization, and it
//! is re-checked against durable state on every single request so that a
//! revocation takes effect on the next one.
//!
//! Nothing on the public surface can reach anything but the one object its
//! capability names, and nothing on it discloses a bucket, a key path, a version
//! identifier, a node, or any other internal fact about how Record Store stores things.

use axum::{
    body::Body,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use record_store_core::{
    BucketName, CONTENT_SIGNATURE_PROBE_BYTES, ObjectKey, ObjectMetadata, PreviewKind,
    content_signature_matches,
};
use record_store_service::ServiceGetResult;
use record_store_sharing::{
    CapabilityTarget, EmbedLink, OriginDecision, VersionMode, matching_origin,
};
use tracing::error;

use crate::AppState;
use crate::dto::RequestId;
use crate::error::ApiError;
use crate::handlers::objects::insert_header;

use crate::sharing::public::EMBED_CONTENT_POLICY;
use crate::sharing::support::target_error;

// ---------------------------------------------------------------------------
// Shared machinery
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Disposition {
    Inline,
    Attachment,
}

pub(crate) fn apply_embed_headers(
    headers: &mut header::HeaderMap,
    link: &EmbedLink,
    decision: OriginDecision,
    presented_origin: Option<&str>,
    metadata: &ObjectMetadata,
) {
    apply_cors(headers, link, decision, presented_origin);
    insert_header(
        headers,
        header::ETAG,
        &format!("\"{}\"", metadata.etag.as_str()),
    );
    insert_header(
        headers,
        header::CONTENT_SECURITY_POLICY,
        EMBED_CONTENT_POLICY,
    );
    // Caching is a real trade rather than an optimisation. An embed is a public
    // asset URL and benefits from being cached; it is also revocable, and a
    // cached copy keeps working until it expires. A minute is short enough that
    // a revocation is effective almost immediately and long enough to absorb the
    // burst of requests one page load produces.
    let cache = if link.origin_restricted() {
        "private, max-age=60, must-revalidate"
    } else {
        "public, max-age=60, must-revalidate"
    };
    insert_header(headers, header::CACHE_CONTROL, cache);
}

/// Emits CORS headers matching the embed's own allowlist.
///
/// The value is always a stored, normalized origin or the explicit wildcard an
/// unrestricted embed already implies. A caller's `Origin` header is never
/// echoed back, so a malformed or hostile value cannot become a grant.
pub(crate) fn apply_cors(
    headers: &mut header::HeaderMap,
    link: &EmbedLink,
    decision: OriginDecision,
    presented_origin: Option<&str>,
) {
    match decision {
        OriginDecision::Allowed => {
            // `Vary` matters here: without it a cache could serve one site's
            // permitted response to another site's request.
            insert_header(headers, header::VARY, "Origin");
            if let Some(origin) = presented_origin
                .and_then(|presented| matching_origin(&link.allowed_origins, presented))
            {
                insert_header(
                    headers,
                    header::ACCESS_CONTROL_ALLOW_ORIGIN,
                    origin.as_str(),
                );
            }
            insert_header(
                headers,
                header::HeaderName::from_static("cross-origin-resource-policy"),
                "cross-origin",
            );
        }
        OriginDecision::Unrestricted => {
            // The operator declined to restrict this embed, so any site may read
            // it. Saying so plainly is more honest than reflecting whatever
            // origin happened to ask.
            insert_header(headers, header::ACCESS_CONTROL_ALLOW_ORIGIN, "*");
            insert_header(
                headers,
                header::HeaderName::from_static("cross-origin-resource-policy"),
                "cross-origin",
            );
        }
        // A restricted embed reached without an `Origin` is a non-browser
        // client. It gets the bytes and no grant, because there is no browser
        // to grant anything to.
        OriginDecision::NoOriginPresented | OriginDecision::Denied => {
            insert_header(headers, header::VARY, "Origin");
        }
    }
}

/// Builds a streaming byte response with the headers its content type earns.
pub(crate) fn byte_response(
    result: ServiceGetResult,
    metadata: &ObjectMetadata,
    disposition: Disposition,
    ranges_allowed: bool,
) -> Response {
    let length = result
        .range
        .map_or(metadata.size, |resolved| resolved.length);
    let status = if result.range.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let content_type = match disposition {
        // An attachment is never interpreted, so the safest possible type is
        // also the correct one: the browser is being asked to save bytes.
        Disposition::Attachment => "application/octet-stream".to_owned(),
        Disposition::Inline => PreviewKind::canonical_content_type(
            metadata.content_type.as_deref().unwrap_or_default(),
        )
        .unwrap_or("application/octet-stream")
        .to_owned(),
    };
    let mut response = (
        status,
        Body::from_stream(futures_util::TryStreamExt::map_err(
            result.body,
            std::io::Error::other,
        )),
    )
        .into_response();
    let headers = response.headers_mut();
    insert_header(headers, header::CONTENT_TYPE, &content_type);
    insert_header(headers, header::CONTENT_LENGTH, &length.to_string());
    insert_header(headers, header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    // A share with a strict access budget refuses ranges, and says so, rather
    // than accepting a range request and quietly serving the whole object.
    insert_header(
        headers,
        header::ACCEPT_RANGES,
        if ranges_allowed { "bytes" } else { "none" },
    );
    let file_name = metadata
        .key
        .as_str()
        .rsplit('/')
        .next()
        .unwrap_or("download")
        .replace(['\\', '"'], "_");
    insert_header(
        headers,
        header::CONTENT_DISPOSITION,
        &match disposition {
            Disposition::Attachment => format!("attachment; filename=\"{file_name}\""),
            Disposition::Inline => format!("inline; filename=\"{file_name}\""),
        },
    );
    if let Some(range) = result.range {
        insert_header(
            headers,
            header::CONTENT_RANGE,
            &format!(
                "bytes {}-{}/{}",
                range.offset,
                range.offset + range.length - 1,
                metadata.size
            ),
        );
    }
    response
}

/// Opens the authoritative object stream for a capability's target.
pub(crate) async fn open_stream(
    state: &AppState,
    target: &CapabilityTarget,
    range: Option<record_store_core::ByteRange>,
    request_id: &RequestId,
) -> Result<ServiceGetResult, ApiError> {
    let objects = &state.services.objects;
    let result = match target.version {
        VersionMode::Pinned { version_id } => {
            objects
                .get_version(&target.bucket, target.key.clone(), version_id, range)
                .await
        }
        VersionMode::FollowCurrent => objects.get(&target.bucket, target.key.clone(), range).await,
    };
    result.map_err(|error| target_error(error, request_id.clone()))
}

/// Reads the metadata of exactly the version a capability names.
pub(crate) async fn read_metadata(
    state: &AppState,
    bucket: &BucketName,
    key: &ObjectKey,
    version: VersionMode,
    request_id: &RequestId,
) -> Result<ObjectMetadata, ApiError> {
    let objects = &state.services.objects;
    let result = match version {
        VersionMode::Pinned { version_id } => {
            objects.head_version(bucket, key.clone(), version_id).await
        }
        VersionMode::FollowCurrent => objects.head(bucket, key.clone()).await,
    };
    result.map_err(|error| target_error(error, request_id.clone()))
}

/// Confirms the object's leading bytes agree with the media type it claims.
///
/// The stored `Content-Type` came from whoever uploaded the object, so serving
/// it inline on the strength of that label alone would let an uploader choose
/// how a browser interprets their bytes. The probe is a fixed handful of bytes
/// from the front of the same version that is about to be served.
pub(crate) async fn verify_signature(
    state: &AppState,
    target: &CapabilityTarget,
    metadata: &ObjectMetadata,
    request_id: &RequestId,
) -> Result<(), ApiError> {
    let Some(content_type) = metadata.content_type.as_deref() else {
        return Ok(());
    };
    if metadata.size == 0 {
        return Ok(());
    }
    let probe_length = metadata.size.min(CONTENT_SIGNATURE_PROBE_BYTES as u64);
    let Ok(range) = record_store_core::ByteRange::new(0, probe_length) else {
        return Ok(());
    };
    let result = open_stream(state, target, Some(range), request_id).await?;
    let prefix = read_probe(result).await.map_err(|error| {
        error!(%error, request_id = %request_id, "content signature probe failed");
        ApiError::internal(request_id.clone())
    })?;
    if content_signature_matches(content_type, &prefix) {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "CONTENT_TYPE_MISMATCH",
            "This object's contents do not match its recorded media type, so it will not be shown inline",
            request_id.clone(),
        ))
    }
}

/// Reads at most one probe's worth of bytes from a stream.
///
/// Bounded explicitly rather than trusting the range: this is the one place the
/// preview and capability paths buffer anything at all, and the bound is what
/// keeps that true regardless of what the storage layer returns.
pub(crate) async fn read_probe(
    result: ServiceGetResult,
) -> Result<Vec<u8>, record_store_storage::StorageError> {
    use futures_util::StreamExt;

    let mut body = result.body;
    let mut prefix = Vec::with_capacity(CONTENT_SIGNATURE_PROBE_BYTES);
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        let remaining = CONTENT_SIGNATURE_PROBE_BYTES.saturating_sub(prefix.len());
        if remaining == 0 {
            break;
        }
        prefix.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if prefix.len() >= CONTENT_SIGNATURE_PROBE_BYTES {
            break;
        }
    }
    Ok(prefix)
}
