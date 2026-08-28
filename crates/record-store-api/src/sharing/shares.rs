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
    Json,
    extract::{Extension, Path, State},
    http::StatusCode,
};
use chrono::Utc;
use record_store_audit::AuditResult;
use record_store_sharing::CreateShareRequest;

use crate::AppState;
use crate::dto::RequestId;
use crate::error::ApiError;

use crate::sharing::dto::{
    CapabilityUrlResponse, CreateShareBody, IssuedShareResponse, ShareResponse,
};
use crate::sharing::respond::read_metadata;
use crate::sharing::support::{
    describe_content_type, parse_share_id, record_capability_audit, require_sharing,
    resolve_target, share_not_found, sharing_to_api_error, version_mode,
};

pub(crate) async fn list_object_shares(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<Vec<ShareResponse>>, ApiError> {
    let sharing = require_sharing(&state, &request_id)?;
    let (bucket_id, _, key) = resolve_target(&state, &bucket, &key, &request_id).await?;
    let now = Utc::now();
    sharing
        .service()
        .store()
        .list_shares_for_object(bucket_id, &key)
        .await
        .map(|links| {
            Json(
                links
                    .iter()
                    .map(|link| ShareResponse::of(link, now))
                    .collect(),
            )
        })
        .map_err(|error| sharing_to_api_error(&error, request_id))
}

pub(crate) async fn create_object_share(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<crate::ManagementPrincipal>,
    Json(body): Json<CreateShareBody>,
) -> Result<(StatusCode, Json<IssuedShareResponse>), ApiError> {
    let sharing = require_sharing(&state, &request_id)?;
    let (bucket_id, bucket_name, object_key) =
        resolve_target(&state, &bucket, &key, &request_id).await?;
    let version = version_mode(body.version_id);
    // The target must exist before a capability is minted for it, and the exact
    // version has to be the one that was asked for: a share pinned to a version
    // that never existed is a link that fails only once someone opens it.
    let metadata = read_metadata(&state, &bucket_name, &object_key, version, &request_id).await?;
    let now = Utc::now();
    let issued = sharing
        .service()
        .create_share(
            CreateShareRequest {
                label: body.label,
                bucket_id,
                bucket: bucket_name,
                key: object_key,
                version,
                permission: body.permission,
                expires_at: body.expires_at,
                password: body.password,
                maximum_access_count: body.maximum_access_count,
                created_by: principal.audit_name().to_owned(),
            },
            now,
        )
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id.clone()))?;
    let url = sharing.share_url(&issued.token);
    record_capability_audit(
        &state,
        &request_id,
        principal,
        "share.created",
        &issued.link.target,
        AuditResult::Success,
        [
            ("share_id", issued.link.id.to_string()),
            ("permission", issued.link.permission.label().to_owned()),
            ("version_mode", version.label().to_owned()),
            (
                "expires_at",
                issued
                    .link
                    .expires_at
                    .map_or_else(|| "never".to_owned(), |at| at.to_rfc3339()),
            ),
            (
                "password_protected",
                issued.link.password_protected().to_string(),
            ),
            ("content_type", describe_content_type(&metadata)),
        ],
    )
    .await;
    state.sharing_metrics.shares_created();
    Ok((
        StatusCode::CREATED,
        Json(IssuedShareResponse {
            share: ShareResponse::of(&issued.link, now),
            url,
        }),
    ))
}

pub(crate) async fn get_share(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<ShareResponse>, ApiError> {
    let sharing = require_sharing(&state, &request_id)?;
    let id = parse_share_id(&id, &request_id)?;
    let link = sharing
        .service()
        .store()
        .get_share(id)
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id.clone()))?
        .ok_or_else(|| share_not_found(request_id))?;
    Ok(Json(ShareResponse::of(&link, Utc::now())))
}

pub(crate) async fn get_share_url(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<CapabilityUrlResponse>, ApiError> {
    let sharing = require_sharing(&state, &request_id)?;
    let id = parse_share_id(&id, &request_id)?;
    if sharing
        .service()
        .store()
        .get_share(id)
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id.clone()))?
        .is_none()
    {
        return Err(share_not_found(request_id));
    }
    let token = sharing
        .service()
        .store()
        .reveal_share_token(id)
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id))?;
    Ok(Json(CapabilityUrlResponse {
        url: token.as_ref().map(|token| sharing.share_url(token)),
        available: token.is_some(),
    }))
}

pub(crate) async fn revoke_share(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<crate::ManagementPrincipal>,
) -> Result<Json<ShareResponse>, ApiError> {
    let sharing = require_sharing(&state, &request_id)?;
    let id = parse_share_id(&id, &request_id)?;
    let now = Utc::now();
    let link = sharing
        .service()
        .revoke_share(id, now)
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id.clone()))?
        .ok_or_else(|| share_not_found(request_id.clone()))?;
    record_capability_audit(
        &state,
        &request_id,
        principal,
        "share.revoked",
        &link.target,
        AuditResult::Success,
        [
            ("share_id", link.id.to_string()),
            ("access_count", link.access_count.to_string()),
        ],
    )
    .await;
    Ok(Json(ShareResponse::of(&link, now)))
}

pub(crate) async fn delete_share(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<crate::ManagementPrincipal>,
) -> Result<StatusCode, ApiError> {
    let sharing = require_sharing(&state, &request_id)?;
    let id = parse_share_id(&id, &request_id)?;
    let now = Utc::now();
    let link = sharing
        .service()
        .store()
        .get_share(id)
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id.clone()))?
        .ok_or_else(|| share_not_found(request_id.clone()))?;
    // Deleting the record deletes the evidence that the link existed, so it is
    // only offered once the link is already inert. An operator who wants a live
    // share gone revokes it, which is authoritative immediately; tidying the
    // history afterwards is a separate, weaker action.
    if link.status(now).usable() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "SHARE_STILL_ACTIVE",
            "Revoke this share before deleting its record",
            request_id,
        ));
    }
    sharing
        .service()
        .store()
        .delete_share(id)
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id.clone()))?;
    record_capability_audit(
        &state,
        &request_id,
        principal,
        "share.deleted",
        &link.target,
        AuditResult::Success,
        [("share_id", link.id.to_string())],
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}
