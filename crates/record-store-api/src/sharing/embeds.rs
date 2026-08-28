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
use record_store_sharing::CreateEmbedRequest;

use crate::AppState;
use crate::dto::RequestId;
use crate::error::ApiError;

use crate::sharing::dto::{
    CapabilityUrlResponse, CreateEmbedBody, EmbedResponse, IssuedEmbedResponse, UpdateEmbedBody,
};
use crate::sharing::respond::read_metadata;
use crate::sharing::support::{
    embed_not_found, parse_embed_id, record_capability_audit, require_sharing, resolve_target,
    sharing_to_api_error, version_mode,
};

pub(crate) async fn list_object_embeds(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<Vec<EmbedResponse>>, ApiError> {
    let sharing = require_sharing(&state, &request_id)?;
    let (bucket_id, _, key) = resolve_target(&state, &bucket, &key, &request_id).await?;
    let now = Utc::now();
    sharing
        .service()
        .store()
        .list_embeds_for_object(bucket_id, &key)
        .await
        .map(|links| {
            Json(
                links
                    .iter()
                    .map(|link| EmbedResponse::of(link, now))
                    .collect(),
            )
        })
        .map_err(|error| sharing_to_api_error(&error, request_id))
}

pub(crate) async fn create_object_embed(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<crate::ManagementPrincipal>,
    Json(body): Json<CreateEmbedBody>,
) -> Result<(StatusCode, Json<IssuedEmbedResponse>), ApiError> {
    let sharing = require_sharing(&state, &request_id)?;
    let (bucket_id, bucket_name, object_key) =
        resolve_target(&state, &bucket, &key, &request_id).await?;
    let version = version_mode(body.version_id);
    let metadata = read_metadata(&state, &bucket_name, &object_key, version, &request_id).await?;
    let now = Utc::now();
    let issued = sharing
        .service()
        .create_embed(
            CreateEmbedRequest {
                label: body.label,
                bucket_id,
                bucket: bucket_name,
                key: object_key,
                version,
                expires_at: body.expires_at,
                allowed_origins: body.allowed_origins,
                disposition: body.disposition,
                content_type: metadata.content_type.clone(),
                created_by: principal.audit_name().to_owned(),
            },
            now,
        )
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id.clone()))?;
    let url = sharing.embed_url(&issued.token);
    record_capability_audit(
        &state,
        &request_id,
        principal,
        "embed.created",
        &issued.link.target,
        AuditResult::Success,
        [
            ("embed_id", issued.link.id.to_string()),
            ("version_mode", version.label().to_owned()),
            ("disposition", issued.link.disposition.label().to_owned()),
            ("content_type", issued.link.content_type.clone()),
            (
                "allowed_origins",
                issued.link.allowed_origins.len().to_string(),
            ),
        ],
    )
    .await;
    state.sharing_metrics.embeds_created();
    Ok((
        StatusCode::CREATED,
        Json(IssuedEmbedResponse {
            embed: EmbedResponse::of(&issued.link, now),
            url,
        }),
    ))
}

pub(crate) async fn get_embed(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<EmbedResponse>, ApiError> {
    let sharing = require_sharing(&state, &request_id)?;
    let id = parse_embed_id(&id, &request_id)?;
    let link = sharing
        .service()
        .store()
        .get_embed(id)
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id.clone()))?
        .ok_or_else(|| embed_not_found(request_id))?;
    Ok(Json(EmbedResponse::of(&link, Utc::now())))
}

pub(crate) async fn get_embed_url(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<CapabilityUrlResponse>, ApiError> {
    let sharing = require_sharing(&state, &request_id)?;
    let id = parse_embed_id(&id, &request_id)?;
    if sharing
        .service()
        .store()
        .get_embed(id)
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id.clone()))?
        .is_none()
    {
        return Err(embed_not_found(request_id));
    }
    let token = sharing
        .service()
        .store()
        .reveal_embed_token(id)
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id))?;
    Ok(Json(CapabilityUrlResponse {
        url: token.as_ref().map(|token| sharing.embed_url(token)),
        available: token.is_some(),
    }))
}

pub(crate) async fn update_embed(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<crate::ManagementPrincipal>,
    Json(body): Json<UpdateEmbedBody>,
) -> Result<Json<EmbedResponse>, ApiError> {
    let sharing = require_sharing(&state, &request_id)?;
    let id = parse_embed_id(&id, &request_id)?;
    let existing = sharing
        .service()
        .store()
        .get_embed(id)
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id.clone()))?
        .ok_or_else(|| embed_not_found(request_id.clone()))?;
    // Dropping every origin turns a restricted embed into one any site may use.
    // That is a widening an operator should have to state outright, so it is
    // refused here rather than applied as if it were an edit.
    if existing.origin_restricted() && body.allowed_origins.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "EMBED_WOULD_BROADEN",
            "Removing every origin restriction widens access. Revoke this embed and create a new one instead.",
            request_id,
        ));
    }
    let now = Utc::now();
    let updated = sharing
        .service()
        .set_embed_origins(id, &body.allowed_origins, now)
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id.clone()))?
        .ok_or_else(|| embed_not_found(request_id.clone()))?;
    record_capability_audit(
        &state,
        &request_id,
        principal,
        "embed.updated",
        &updated.target,
        AuditResult::Success,
        [
            ("embed_id", updated.id.to_string()),
            (
                "previous_origins",
                existing.allowed_origins.len().to_string(),
            ),
            ("allowed_origins", updated.allowed_origins.len().to_string()),
        ],
    )
    .await;
    Ok(Json(EmbedResponse::of(&updated, now)))
}

pub(crate) async fn revoke_embed(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<crate::ManagementPrincipal>,
) -> Result<Json<EmbedResponse>, ApiError> {
    let sharing = require_sharing(&state, &request_id)?;
    let id = parse_embed_id(&id, &request_id)?;
    let now = Utc::now();
    let link = sharing
        .service()
        .revoke_embed(id, now)
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id.clone()))?
        .ok_or_else(|| embed_not_found(request_id.clone()))?;
    record_capability_audit(
        &state,
        &request_id,
        principal,
        "embed.revoked",
        &link.target,
        AuditResult::Success,
        [("embed_id", link.id.to_string())],
    )
    .await;
    Ok(Json(EmbedResponse::of(&link, now)))
}

pub(crate) async fn delete_embed(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<crate::ManagementPrincipal>,
) -> Result<StatusCode, ApiError> {
    let sharing = require_sharing(&state, &request_id)?;
    let id = parse_embed_id(&id, &request_id)?;
    let now = Utc::now();
    let link = sharing
        .service()
        .store()
        .get_embed(id)
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id.clone()))?
        .ok_or_else(|| embed_not_found(request_id.clone()))?;
    if link.status(now).usable() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "EMBED_STILL_ACTIVE",
            "Revoke this embed before deleting its record",
            request_id,
        ));
    }
    sharing
        .service()
        .store()
        .delete_embed(id)
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id.clone()))?;
    record_capability_audit(
        &state,
        &request_id,
        principal,
        "embed.deleted",
        &link.target,
        AuditResult::Success,
        [("embed_id", link.id.to_string())],
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------
