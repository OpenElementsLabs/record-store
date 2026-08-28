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
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::Utc;
use record_store_audit::{AuditEvent, AuditResult};
use record_store_core::{
    BucketName, EmbedLinkId, ObjectKey, ObjectMetadata, PreviewKind, ShareLinkId, VersionId,
};
use record_store_service::ServiceError;
use record_store_sharing::{
    AccessDenial, AccessRefusal, CapabilityTarget, ShareLink, SharingError, VersionMode,
};
use tracing::error;

use crate::AppState;
use crate::dto::RequestId;
use crate::error::{ApiError, service_to_api_error};
use crate::handlers::objects::{insert_header, parse_bucket_name, parse_object_key};

use crate::sharing::dto::PublicShareResponse;
use crate::sharing::respond::read_metadata;
use crate::sharing::*;

pub(crate) async fn describe_share(
    state: &AppState,
    link: &ShareLink,
    sharing: &SharingManagement,
    request_id: &RequestId,
) -> Result<PublicShareResponse, ApiError> {
    let metadata = read_metadata(
        state,
        &link.target.bucket,
        &link.target.key,
        link.target.version,
        request_id,
    )
    .await?;
    let kind = PreviewKind::classify(metadata.content_type.as_deref());
    let inline_safe = kind.allows_inline();
    Ok(PublicShareResponse {
        state: "open",
        file_name: Some(link.target.file_name().to_owned()),
        content_type: if inline_safe {
            PreviewKind::canonical_content_type(
                metadata.content_type.as_deref().unwrap_or_default(),
            )
            .map(str::to_owned)
        } else {
            None
        },
        size: Some(metadata.size),
        preview: Some(kind.label()),
        can_view: link.permission.allows_view() && inline_safe,
        can_download: link.permission.allows_download(),
        expires_at: link.expires_at,
        preview_text_limit_bytes: sharing.preview_text_limit_bytes(),
    })
}

pub(crate) fn private_json<T: IntoResponse>(payload: T) -> Response {
    let mut response = payload.into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store, max-age=0"),
    );
    response
}

pub(crate) fn throttled_response(retry_after_seconds: u64, request_id: &RequestId) -> Response {
    let mut response = ApiError::new(
        StatusCode::TOO_MANY_REQUESTS,
        "RATE_LIMITED",
        "Too many attempts. Try again shortly.",
        request_id.clone(),
    )
    .into_response();
    insert_header(
        response.headers_mut(),
        header::RETRY_AFTER,
        &retry_after_seconds.to_string(),
    );
    response
}

pub(crate) async fn denial_response(
    state: &AppState,
    request_id: &RequestId,
    operation: &'static str,
    denial: AccessDenial,
) -> Response {
    match denial {
        AccessDenial::Throttled {
            retry_after_seconds,
        } => {
            record_public_denial(state, request_id, operation, None, "throttled").await;
            throttled_response(retry_after_seconds, request_id)
        }
        AccessDenial::PasswordRequired => ApiError::new(
            StatusCode::UNAUTHORIZED,
            "SHARE_PASSWORD_REQUIRED",
            "This link is password protected",
            request_id.clone(),
        )
        .into_response(),
        AccessDenial::NotPermitted => {
            record_public_denial(state, request_id, operation, None, "not_permitted").await;
            ApiError::new(
                StatusCode::FORBIDDEN,
                "SHARE_NOT_PERMITTED",
                "This link does not permit that",
                request_id.clone(),
            )
            .into_response()
        }
        AccessDenial::OriginDenied => {
            record_public_denial(state, request_id, operation, None, "origin_denied").await;
            ApiError::new(
                StatusCode::FORBIDDEN,
                "EMBED_ORIGIN_DENIED",
                "This embed is not permitted on this site",
                request_id.clone(),
            )
            .into_response()
        }
        AccessDenial::Unavailable(refusal) => {
            if matches!(refusal, AccessRefusal::NotUsable(_)) {
                record_public_denial(state, request_id, operation, None, refusal_label(refusal))
                    .await;
            }
            if operation.starts_with("embed") {
                embed_unavailable(request_id.clone()).into_response()
            } else {
                share_unavailable(request_id.clone()).into_response()
            }
        }
    }
}

pub(crate) const fn refusal_label(refusal: AccessRefusal) -> &'static str {
    match refusal {
        AccessRefusal::Unknown => "unknown_token",
        AccessRefusal::NotUsable(status) => status.label(),
    }
}

/// Records a denied public access as a security event.
///
/// Only denials involving a capability Record Store actually issued are recorded. A
/// stranger guessing tokens produces no audit entries at all, because letting an
/// anonymous caller write unbounded rows into the security trail would itself be
/// the vulnerability. Successful accesses are counted as metrics rather than
/// audited, so a video's byte ranges never become a million immutable records.
pub(crate) async fn record_public_denial(
    state: &AppState,
    request_id: &RequestId,
    operation: &'static str,
    target: Option<&CapabilityTarget>,
    reason: &'static str,
) {
    let event = AuditEvent {
        event_id: record_store_core::AuditEventId::new(),
        timestamp: Utc::now(),
        request_id: Some(request_id.to_string()),
        // A public visitor has no identity, and inventing one would be worse
        // than saying so.
        principal: "capability:public".to_owned(),
        credential_id: None,
        source_ip: None,
        operation: operation.to_owned(),
        resource: target.map_or_else(|| "capability".to_owned(), CapabilityTarget::audit_resource),
        result: AuditResult::Denied,
        metadata: [("reason".to_owned(), reason.to_owned())]
            .into_iter()
            .collect(),
    };
    if let Err(error) = state.audit.append(&event).await {
        error!(%error, request_id = %request_id, "durable audit append failed");
    }
}

pub(crate) async fn record_capability_audit<const N: usize>(
    state: &AppState,
    request_id: &RequestId,
    principal: crate::ManagementPrincipal,
    operation: &'static str,
    target: &CapabilityTarget,
    result: AuditResult,
    metadata: [(&'static str, String); N],
) {
    let event = AuditEvent {
        event_id: record_store_core::AuditEventId::new(),
        timestamp: Utc::now(),
        request_id: Some(request_id.to_string()),
        principal: principal.audit_name().to_owned(),
        credential_id: None,
        source_ip: None,
        operation: operation.to_owned(),
        resource: target.audit_resource(),
        result,
        metadata: metadata
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    };
    if let Err(error) = state.audit.append(&event).await {
        error!(%error, request_id = %request_id, "durable audit append failed");
    }
}

pub(crate) fn describe_content_type(metadata: &ObjectMetadata) -> String {
    metadata
        .content_type
        .clone()
        .unwrap_or_else(|| "application/octet-stream".to_owned())
}

pub(crate) const fn version_mode(version_id: Option<VersionId>) -> VersionMode {
    match version_id {
        Some(version_id) => VersionMode::Pinned { version_id },
        None => VersionMode::FollowCurrent,
    }
}

pub(crate) async fn resolve_target(
    state: &AppState,
    bucket: &str,
    key: &str,
    request_id: &RequestId,
) -> Result<(record_store_core::BucketId, BucketName, ObjectKey), ApiError> {
    let name = parse_bucket_name(bucket, request_id)?;
    let key = parse_object_key(key, request_id)?;
    let bucket = state
        .services
        .buckets
        .head(&name)
        .await
        .map_err(|error| service_to_api_error(error, request_id.clone()))?;
    Ok((bucket.id, name, key))
}

pub(crate) fn require_sharing<'a>(
    state: &'a AppState,
    request_id: &RequestId,
) -> Result<&'a SharingManagement, ApiError> {
    state.sharing.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "SHARING_UNAVAILABLE",
            "Sharing is not enabled on this deployment",
            request_id.clone(),
        )
    })
}

pub(crate) fn parse_share_id(value: &str, request_id: &RequestId) -> Result<ShareLinkId, ApiError> {
    value.parse().map_err(|_| {
        ApiError::bad_request(request_id.clone(), "INVALID_SHARE_ID", "Invalid share ID")
    })
}

pub(crate) fn parse_embed_id(value: &str, request_id: &RequestId) -> Result<EmbedLinkId, ApiError> {
    value.parse().map_err(|_| {
        ApiError::bad_request(request_id.clone(), "INVALID_EMBED_ID", "Invalid embed ID")
    })
}

pub(crate) fn share_not_found(request_id: RequestId) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "SHARE_NOT_FOUND",
        "Share link was not found",
        request_id,
    )
}

pub(crate) fn embed_not_found(request_id: RequestId) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "EMBED_NOT_FOUND",
        "Embed link was not found",
        request_id,
    )
}

/// The single answer a public visitor gets for every unusable share.
///
/// Unknown, revoked, expired, and exhausted are deliberately indistinguishable
/// from outside. Telling a stranger that their guess named a real link that has
/// since expired confirms the guess, which is most of what they wanted.
pub(crate) fn share_unavailable(request_id: RequestId) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "SHARE_UNAVAILABLE",
        "This link is not available",
        request_id,
    )
}

pub(crate) fn embed_unavailable(request_id: RequestId) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "EMBED_UNAVAILABLE",
        "This embed is not available",
        request_id,
    )
}

/// Maps a failure to read the object a capability points at.
///
/// A public caller learns only that the thing is not there. Whether the bucket
/// was deleted, the version was purged, or a delete marker now hides it is
/// internal history, and none of it is theirs to know.
pub(crate) fn target_error(error: ServiceError, request_id: RequestId) -> ApiError {
    match error {
        ServiceError::BucketNotFound | ServiceError::ObjectNotFound => ApiError::new(
            StatusCode::NOT_FOUND,
            "OBJECT_NOT_FOUND",
            "Object was not found",
            request_id,
        ),
        ServiceError::DeleteMarker(_) => ApiError::new(
            StatusCode::NOT_FOUND,
            "OBJECT_DELETED",
            "This version of the object has been deleted",
            request_id,
        ),
        other => service_to_api_error(other, request_id),
    }
}

pub(crate) fn sharing_to_api_error(error: &SharingError, request_id: RequestId) -> ApiError {
    match error {
        SharingError::Invalid(reason) | SharingError::InvalidPassword(reason) => ApiError::new(
            StatusCode::BAD_REQUEST,
            "INVALID_CAPABILITY_REQUEST",
            reason.clone(),
            request_id,
        ),
        SharingError::InvalidOrigin(reason) => ApiError::new(
            StatusCode::BAD_REQUEST,
            "INVALID_ORIGIN",
            reason.clone(),
            request_id,
        ),
        SharingError::PolicyRefused(reason) => ApiError::new(
            StatusCode::FORBIDDEN,
            "CAPABILITY_REFUSED",
            reason.clone(),
            request_id,
        ),
        other => {
            // Storage, cryptography, and entropy failures are operational and
            // must be diagnosable from the logs without any of their detail
            // reaching a public visitor.
            error!(error = %other, request_id = %request_id, "capability operation failed");
            ApiError::internal(request_id)
        }
    }
}
