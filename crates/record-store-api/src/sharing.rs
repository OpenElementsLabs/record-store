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

use std::{net::SocketAddr, sync::Arc};

use axum::{
    Json,
    body::Body,
    extract::{ConnectInfo, Extension, Path, Query, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use record_store_audit::{AuditEvent, AuditResult};
use record_store_core::{
    BucketName, CONTENT_SIGNATURE_PROBE_BYTES, EmbedLinkId, ObjectKey, ObjectMetadata, PreviewKind,
    ShareLinkId, VersionId, content_signature_matches,
};
use record_store_service::{ServiceError, ServiceGetResult};
use record_store_sharing::{
    AccessDenial, AccessRefusal, CapabilityStatus, CapabilityTarget, CapabilityToken,
    CreateEmbedRequest, CreateShareRequest, EmbedDisposition, EmbedLink, OriginDecision,
    RateDecision, ShareLink, ShareLookup, SharePermission, SharingError, SharingService,
    UnlockFailure, VersionMode, matching_origin,
};
use serde::{Deserialize, Serialize};
use tracing::error;

use crate::{
    ApiError, AppState, ClientIdentity, RequestId, insert_header, parse_bucket_name,
    parse_object_key, service_to_api_error,
};

/// The sharing dependencies an API instance needs.
#[derive(Clone)]
pub struct SharingManagement {
    service: Arc<SharingService>,
    share_base_url: Option<String>,
    embed_base_url: String,
    preview_text_limit_bytes: u64,
}

impl SharingManagement {
    /// Creates the management surface from a running sharing service.
    ///
    /// The two base addresses are separate because the two capabilities are
    /// published in different places. A share link is a page a person opens, so
    /// it lives on the console; an embed serves object bytes into somebody
    /// else's page, so it lives on the storage endpoint. Collapsing them would
    /// either route asset traffic through the administrative console or publish
    /// the console's address to every site that embeds an image.
    #[must_use]
    pub fn new(
        service: Arc<SharingService>,
        share_base_url: Option<String>,
        embed_base_url: String,
        preview_text_limit_bytes: u64,
    ) -> Self {
        Self {
            service,
            share_base_url,
            embed_base_url,
            preview_text_limit_bytes,
        }
    }

    /// Returns the capability service.
    #[must_use]
    pub fn service(&self) -> &SharingService {
        &self.service
    }

    /// Returns the configured preview slice size.
    #[must_use]
    pub const fn preview_text_limit_bytes(&self) -> u64 {
        self.preview_text_limit_bytes
    }

    /// Builds the URL a share recipient opens.
    ///
    /// Without a configured base this returns only the path. That is not a
    /// failure: the console knows its own public origin and completes the URL,
    /// and guessing an external address from a request header would be a way to
    /// hand out links pointing at somewhere Record Store was never deployed.
    fn share_url(&self, token: &CapabilityToken) -> String {
        match &self.share_base_url {
            Some(base) => format!("{base}/s/{}", token.expose()),
            None => format!("/s/{}", token.expose()),
        }
    }

    /// Builds the URL a website loads an embed from.
    ///
    /// Always absolute, because the browser that eventually resolves it is on a
    /// page Record Store has nothing to do with: there is no origin for it to fall back
    /// to. The address is the storage endpoint, resolved once at startup.
    fn embed_url(&self, token: &CapabilityToken) -> String {
        format!("{}/e/{}", self.embed_base_url, token.expose())
    }
}

/// Extracts the identity abuse controls are applied to.
///
/// `X-Forwarded-For` is honoured because public capability traffic reaches Record Store
/// through the console or a reverse proxy, and the socket address would
/// otherwise be that hop for every visitor in the world. The header is only
/// meaningful when the management listener is not itself internet-facing, which
/// is how Record Store is meant to be deployed; when it is absent the socket address is
/// used and the limits simply apply more coarsely. The value is bounded and
/// sanitised because it is attacker-influenced either way, and it is never used
/// for anything but partitioning a counter.
pub(crate) fn client_identity(
    headers: &header::HeaderMap,
    connect: Option<&ConnectInfo<SocketAddr>>,
) -> String {
    let forwarded = headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b".:[]-_".contains(&byte))
        });
    match forwarded {
        Some(value) => value.to_owned(),
        None => connect.map_or_else(|| "unknown".to_owned(), |info| info.0.ip().to_string()),
    }
}

// ---------------------------------------------------------------------------
// Management surface
// ---------------------------------------------------------------------------

/// What a deployment permits, so the console offers only what will be accepted.
#[derive(Debug, Serialize)]
pub(crate) struct SharingSettingsResponse {
    shares_enabled: bool,
    embeds_enabled: bool,
    maximum_lifetime_days: Option<i64>,
    require_expiration: bool,
    require_share_password: bool,
    maximum_access_count: u32,
    minimum_password_length: usize,
    preview_text_limit_bytes: u64,
    /// Element embeds Record Store will accept, so the console can explain a refusal
    /// before the operator fills in a dialog.
    embeddable_content_types: Vec<&'static str>,
}

pub(crate) async fn sharing_settings(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<SharingSettingsResponse>, ApiError> {
    let sharing = require_sharing(&state, &request_id)?;
    let policy = sharing.service().policy();
    Ok(Json(SharingSettingsResponse {
        shares_enabled: policy.shares_enabled,
        embeds_enabled: policy.embeds_enabled,
        maximum_lifetime_days: policy.maximum_lifetime.map(|lifetime| lifetime.num_days()),
        require_expiration: policy.require_expiration,
        require_share_password: policy.require_share_password,
        maximum_access_count: policy.maximum_access_count,
        minimum_password_length: record_store_sharing::MINIMUM_PASSWORD_LENGTH,
        preview_text_limit_bytes: sharing.preview_text_limit_bytes(),
        embeddable_content_types: EMBEDDABLE_CONTENT_TYPES.to_vec(),
    }))
}

/// Media types an inline element embed may be created for.
///
/// Listed rather than derived so the console can show them, and kept in step
/// with the classification by a test rather than by memory.
const EMBEDDABLE_CONTENT_TYPES: &[&str] = &[
    "image/jpeg",
    "image/png",
    "image/webp",
    "image/gif",
    "video/mp4",
    "video/webm",
    "audio/mpeg",
    "audio/ogg",
    "audio/wav",
    "audio/webm",
];

/// A share as the management plane sees it. Never carries the token.
#[derive(Debug, Serialize)]
pub(crate) struct ShareResponse {
    id: ShareLinkId,
    label: String,
    bucket: String,
    key: String,
    version_mode: &'static str,
    version_id: Option<VersionId>,
    permission: SharePermission,
    status: CapabilityStatus,
    password_protected: bool,
    created_by: String,
    created_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
    last_accessed_at: Option<DateTime<Utc>>,
    access_count: u32,
    maximum_access_count: Option<u32>,
}

impl ShareResponse {
    fn of(link: &ShareLink, now: DateTime<Utc>) -> Self {
        Self {
            id: link.id,
            label: link.label.clone(),
            bucket: link.target.bucket.to_string(),
            key: link.target.key.to_string(),
            version_mode: link.target.version.label(),
            version_id: link.target.version.pinned(),
            permission: link.permission,
            status: link.status(now),
            password_protected: link.password_protected(),
            created_by: link.created_by.clone(),
            created_at: link.created_at,
            expires_at: link.expires_at,
            revoked_at: link.revoked_at,
            last_accessed_at: link.last_accessed_at,
            access_count: link.access_count,
            maximum_access_count: link.maximum_access_count,
        }
    }
}

/// An embed as the management plane sees it. Never carries the token.
#[derive(Debug, Serialize)]
pub(crate) struct EmbedResponse {
    id: EmbedLinkId,
    label: String,
    bucket: String,
    key: String,
    version_mode: &'static str,
    version_id: Option<VersionId>,
    status: CapabilityStatus,
    content_type: String,
    disposition: EmbedDisposition,
    allowed_origins: Vec<String>,
    created_by: String,
    created_at: DateTime<Utc>,
    updated_at: Option<DateTime<Utc>>,
    expires_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
    last_accessed_at: Option<DateTime<Utc>>,
    access_count: u64,
}

impl EmbedResponse {
    fn of(link: &EmbedLink, now: DateTime<Utc>) -> Self {
        Self {
            id: link.id,
            label: link.label.clone(),
            bucket: link.target.bucket.to_string(),
            key: link.target.key.to_string(),
            version_mode: link.target.version.label(),
            version_id: link.target.version.pinned(),
            status: link.status(now),
            content_type: link.content_type.clone(),
            disposition: link.disposition,
            allowed_origins: link
                .allowed_origins
                .iter()
                .map(|origin| origin.as_str().to_owned())
                .collect(),
            created_by: link.created_by.clone(),
            created_at: link.created_at,
            updated_at: link.updated_at,
            expires_at: link.expires_at,
            revoked_at: link.revoked_at,
            last_accessed_at: link.last_accessed_at,
            access_count: link.access_count,
        }
    }
}

/// A newly created capability. The URL appears here and in one dedicated route.
#[derive(Debug, Serialize)]
pub(crate) struct IssuedShareResponse {
    share: ShareResponse,
    url: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct IssuedEmbedResponse {
    embed: EmbedResponse,
    url: String,
}

/// The URL of an existing capability, fetched only when it is about to be used.
///
/// Split out of the listing on purpose. A list of shares is read often, by
/// anything from a dashboard to a log-capturing proxy, and putting live tokens
/// in that response would scatter working capabilities across places nobody
/// audited. Copying a link is a deliberate act, so it gets a deliberate request.
#[derive(Debug, Serialize)]
pub(crate) struct CapabilityUrlResponse {
    url: Option<String>,
    /// False when the stored token can no longer be decrypted, which happens
    /// after the deployment's master key changes. The capability still works;
    /// only redisplaying it does not.
    available: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateShareBody {
    label: String,
    #[serde(default)]
    version_id: Option<VersionId>,
    #[serde(default = "default_permission")]
    permission: SharePermission,
    #[serde(default)]
    expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    maximum_access_count: Option<u32>,
}

const fn default_permission() -> SharePermission {
    SharePermission::ViewAndDownload
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateEmbedBody {
    label: String,
    #[serde(default)]
    version_id: Option<VersionId>,
    #[serde(default)]
    expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    allowed_origins: Vec<String>,
    #[serde(default)]
    disposition: EmbedDisposition,
}

#[derive(Debug, Deserialize)]
pub(crate) struct UpdateEmbedBody {
    allowed_origins: Vec<String>,
}

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

/// What a share page may learn about the object behind a link.
///
/// Everything Record Store knows and does not need to say is left out: the bucket, the
/// key's path, the version identifier, the checksum, the storage layout. A
/// recipient needs to recognise the file and decide whether to open it, and
/// that is the whole list.
#[derive(Debug, Serialize)]
pub(crate) struct PublicShareResponse {
    state: &'static str,
    file_name: Option<String>,
    content_type: Option<String>,
    size: Option<u64>,
    preview: Option<&'static str>,
    can_view: bool,
    can_download: bool,
    expires_at: Option<DateTime<Utc>>,
    /// How much of a text object the viewer should read before saying so.
    preview_text_limit_bytes: u64,
}

impl PublicShareResponse {
    fn locked() -> Self {
        Self {
            state: "password_required",
            file_name: None,
            content_type: None,
            size: None,
            preview: None,
            can_view: false,
            can_download: false,
            expires_at: None,
            preview_text_limit_bytes: 0,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct UnlockBody {
    password: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct UnlockResponse {
    ticket: String,
    expires_in_seconds: i64,
}

/// Describes a share to its recipient, or asks for the password first.
pub(crate) async fn public_share_descriptor(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: header::HeaderMap,
    Extension(client): Extension<ClientIdentity>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Response, ApiError> {
    let sharing = require_sharing(&state, &request_id)?;
    let client = client.as_str();
    // Proof of an earlier unlock, if the visitor has one. It is what turns the
    // challenge below into the file they were sent.
    let ticket = headers
        .get("x-record-store-share-ticket")
        .and_then(|value| value.to_str().ok());
    let Some(token) = CapabilityToken::parse(&token) else {
        // A malformed token never reaches the store, but it is still a guess.
        if let RateDecision::Throttled {
            retry_after_seconds,
        } = sharing.service().probe_allowance(client)
        {
            return Ok(throttled_response(retry_after_seconds, &request_id));
        }
        state.sharing_metrics.share_denied();
        return Err(share_unavailable(request_id));
    };
    let now = Utc::now();
    let lookup = sharing
        .service()
        .look_up_share(&token, ticket, client, now)
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id.clone()))?;
    match lookup {
        ShareLookup::Unavailable(refusal) => {
            record_public_denial(
                &state,
                &request_id,
                "share.denied",
                None,
                refusal_label(refusal),
            )
            .await;
            state.sharing_metrics.share_denied();
            Err(share_unavailable(request_id))
        }
        ShareLookup::PasswordRequired(_) => Ok(private_json(Json(PublicShareResponse::locked()))),
        ShareLookup::Open(link) => {
            let descriptor = describe_share(&state, &link, sharing, &request_id).await?;
            Ok(private_json(Json(descriptor)))
        }
    }
}

/// Verifies a share password and hands back a short-lived unlock proof.
pub(crate) async fn public_share_unlock(
    State(state): State<AppState>,
    Path(token): Path<String>,
    Extension(client): Extension<ClientIdentity>,
    Extension(request_id): Extension<RequestId>,
    Json(body): Json<UnlockBody>,
) -> Result<Response, ApiError> {
    let sharing = require_sharing(&state, &request_id)?;
    let client = client.as_str();
    let Some(token) = CapabilityToken::parse(&token) else {
        state.sharing_metrics.share_denied();
        return Err(share_unavailable(request_id));
    };
    let now = Utc::now();
    let outcome = sharing
        .service()
        .unlock_share(&token, &body.password, client, now)
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id.clone()))?;
    match outcome {
        Ok(ticket) => {
            let seconds = sharing.service().policy().unlock_lifetime.num_seconds();
            Ok(private_json(Json(UnlockResponse {
                ticket: ticket.into_string(),
                expires_in_seconds: seconds,
            })))
        }
        Err(UnlockFailure::Throttled {
            retry_after_seconds,
        }) => {
            record_public_denial(
                &state,
                &request_id,
                "share.password_throttled",
                None,
                "throttled",
            )
            .await;
            state.sharing_metrics.share_denied();
            Ok(throttled_response(retry_after_seconds, &request_id))
        }
        Err(UnlockFailure::IncorrectPassword) => {
            record_public_denial(
                &state,
                &request_id,
                "share.password_failed",
                None,
                "incorrect_password",
            )
            .await;
            state.sharing_metrics.share_denied();
            Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                "SHARE_PASSWORD_INCORRECT",
                "That password is not correct",
                request_id,
            ))
        }
        Err(UnlockFailure::NotPasswordProtected | UnlockFailure::Unavailable(_)) => {
            state.sharing_metrics.share_denied();
            Err(share_unavailable(request_id))
        }
    }
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct ShareContentQuery {
    /// Present when the recipient asked to save the file rather than view it.
    #[serde(default)]
    download: bool,
}

/// Streams the bytes behind a share link.
pub(crate) async fn public_share_content(
    State(state): State<AppState>,
    Path(token): Path<String>,
    Query(query): Query<ShareContentQuery>,
    headers: header::HeaderMap,
    Extension(client): Extension<ClientIdentity>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Response, ApiError> {
    let sharing = require_sharing(&state, &request_id)?;
    let client = client.as_str();
    let Some(token) = CapabilityToken::parse(&token) else {
        state.sharing_metrics.share_denied();
        return Err(share_unavailable(request_id));
    };
    let ticket = headers
        .get("x-record-store-share-ticket")
        .and_then(|value| value.to_str().ok());
    let now = Utc::now();
    let authorized = sharing
        .service()
        .authorize_share_access(&token, ticket, query.download, client, now)
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id.clone()))?;
    let link = match authorized {
        Ok(link) => link,
        Err(denial) => {
            state.sharing_metrics.share_denied();
            return Ok(denial_response(&state, &request_id, "share.denied", denial).await);
        }
    };
    let metadata = read_metadata(
        &state,
        &link.target.bucket,
        &link.target.key,
        link.target.version,
        &request_id,
    )
    .await?;
    let kind = PreviewKind::classify(metadata.content_type.as_deref());

    // A share with a strict access budget ignores byte ranges and always serves
    // the whole object. That is what makes "five downloads" mean five: if a
    // client could take the file one range at a time, the counter would measure
    // requests rather than deliveries and the limit would be decorative.
    let budgeted = link.maximum_access_count.is_some();
    let range = if budgeted {
        None
    } else {
        crate::parse_preview_range(&headers, metadata.size, &request_id)?
    };

    let disposition = if query.download {
        Disposition::Attachment
    } else {
        if !kind.allows_inline() {
            state.sharing_metrics.share_denied();
            return Err(ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "SHARE_PREVIEW_UNSUPPORTED",
                "This object cannot be shown safely in a browser. Download it instead.",
                request_id,
            ));
        }
        verify_signature(&state, &link.target, &metadata, &request_id).await?;
        Disposition::Inline
    };

    let result = open_stream(&state, &link.target, range, &request_id).await?;
    state.sharing_metrics.share_access();
    let mut response = byte_response(result, &metadata, disposition, !budgeted);
    let response_headers = response.headers_mut();
    // A share is revocable, so nothing between Record Store and the recipient may keep a
    // copy that outlives the revocation.
    insert_header(
        response_headers,
        header::CACHE_CONTROL,
        "private, no-store, max-age=0",
    );
    insert_header(
        response_headers,
        header::CONTENT_SECURITY_POLICY,
        SHARE_CONTENT_POLICY,
    );
    Ok(response)
}

/// The policy carried by share and preview bytes.
///
/// `sandbox` drops the response into an opaque origin, so a PDF viewer still
/// renders while anything the document tries to do — script, navigation, form
/// submission — has no origin to do it to. `frame-ancestors 'self'` lets the
/// console and the share page frame the viewer and stops any other site from
/// doing so.
pub(crate) const SHARE_CONTENT_POLICY: &str = "sandbox allow-downloads; default-src 'none'; frame-ancestors 'self'; base-uri 'none'; \
     form-action 'none'";

/// The policy carried by embed bytes.
///
/// Embeds are loaded by `<img>`, `<video>`, and `<audio>` on other people's
/// pages, where framing is not the risk. The bytes still get an opaque origin so
/// that a direct navigation to an embed URL cannot execute anything.
pub(crate) const EMBED_CONTENT_POLICY: &str =
    "sandbox; default-src 'none'; base-uri 'none'; form-action 'none'";

/// Streams the bytes behind an embed link.
pub(crate) async fn public_embed_content(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: header::HeaderMap,
    Extension(client): Extension<ClientIdentity>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Response, ApiError> {
    let sharing = require_sharing(&state, &request_id)?;
    let client = client.as_str();
    let presented_origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    let Some(token) = CapabilityToken::parse(&token) else {
        state.sharing_metrics.embed_denied();
        return Err(embed_unavailable(request_id));
    };
    let now = Utc::now();
    let authorized = sharing
        .service()
        .authorize_embed_access(&token, presented_origin, client, now)
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id.clone()))?;
    let (link, decision) = match authorized {
        Ok(granted) => granted,
        Err(denial) => {
            state.sharing_metrics.embed_denied();
            return Ok(denial_response(&state, &request_id, "embed.denied", denial).await);
        }
    };
    let metadata = read_metadata(
        &state,
        &link.target.bucket,
        &link.target.key,
        link.target.version,
        &request_id,
    )
    .await?;
    let kind = PreviewKind::classify(metadata.content_type.as_deref());

    let disposition = match link.disposition {
        EmbedDisposition::Attachment => Disposition::Attachment,
        EmbedDisposition::Inline => {
            // An embed that follows the current version can find that the
            // object has been replaced by something that must not be rendered.
            // The check that was made at creation is therefore made again here,
            // against the version actually about to be served.
            let current = PreviewKind::canonical_content_type(
                metadata.content_type.as_deref().unwrap_or_default(),
            );
            if !kind.allows_inline() || current != Some(link.content_type.as_str()) {
                state.sharing_metrics.embed_denied();
                record_public_denial(
                    &state,
                    &request_id,
                    "embed.denied",
                    Some(&link.target),
                    "content_type_changed",
                )
                .await;
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "EMBED_CONTENT_CHANGED",
                    "This object is no longer the media type this embed was created for",
                    request_id,
                ));
            }
            verify_signature(&state, &link.target, &metadata, &request_id).await?;
            Disposition::Inline
        }
    };

    // An embed is an asset URL that a page reloads constantly, so honouring a
    // revalidation is worth the few lines: the alternative is resending an
    // unchanged image on every visit.
    let revalidated = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|presented| {
            presented
                .split(',')
                .map(str::trim)
                .any(|candidate| candidate.trim_matches('"') == metadata.etag.as_str())
        });
    if revalidated {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        apply_embed_headers(
            response.headers_mut(),
            &link,
            decision,
            presented_origin,
            &metadata,
        );
        state.sharing_metrics.embed_request();
        return Ok(response);
    }

    let range = crate::parse_preview_range(&headers, metadata.size, &request_id)?;
    let result = open_stream(&state, &link.target, range, &request_id).await?;
    state.sharing_metrics.embed_request();
    let mut response = byte_response(result, &metadata, disposition, true);
    apply_embed_headers(
        response.headers_mut(),
        &link,
        decision,
        presented_origin,
        &metadata,
    );
    Ok(response)
}

/// Answers a browser's CORS preflight for an embed.
pub(crate) async fn public_embed_preflight(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: header::HeaderMap,
    Extension(client): Extension<ClientIdentity>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Response, ApiError> {
    let sharing = require_sharing(&state, &request_id)?;
    let client = client.as_str();
    let presented_origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    let Some(token) = CapabilityToken::parse(&token) else {
        return Err(embed_unavailable(request_id));
    };
    let now = Utc::now();
    let authorized = sharing
        .service()
        .authorize_embed_access(&token, presented_origin, client, now)
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id.clone()))?;
    let Ok((link, decision)) = authorized else {
        state.sharing_metrics.embed_denied();
        return Err(embed_unavailable(request_id));
    };
    let mut response = StatusCode::NO_CONTENT.into_response();
    let response_headers = response.headers_mut();
    apply_cors(response_headers, &link, decision, presented_origin);
    insert_header(
        response_headers,
        header::ACCESS_CONTROL_ALLOW_METHODS,
        "GET, HEAD, OPTIONS",
    );
    insert_header(
        response_headers,
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        "range, if-none-match",
    );
    insert_header(response_headers, header::ACCESS_CONTROL_MAX_AGE, "600");
    Ok(response)
}

// ---------------------------------------------------------------------------
// Shared machinery
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Disposition {
    Inline,
    Attachment,
}

fn apply_embed_headers(
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
fn apply_cors(
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
fn byte_response(
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
async fn open_stream(
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
async fn read_metadata(
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
async fn verify_signature(
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

async fn describe_share(
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

fn private_json<T: IntoResponse>(payload: T) -> Response {
    let mut response = payload.into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store, max-age=0"),
    );
    response
}

fn throttled_response(retry_after_seconds: u64, request_id: &RequestId) -> Response {
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

async fn denial_response(
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

const fn refusal_label(refusal: AccessRefusal) -> &'static str {
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
async fn record_public_denial(
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

async fn record_capability_audit<const N: usize>(
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

fn describe_content_type(metadata: &ObjectMetadata) -> String {
    metadata
        .content_type
        .clone()
        .unwrap_or_else(|| "application/octet-stream".to_owned())
}

const fn version_mode(version_id: Option<VersionId>) -> VersionMode {
    match version_id {
        Some(version_id) => VersionMode::Pinned { version_id },
        None => VersionMode::FollowCurrent,
    }
}

async fn resolve_target(
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

fn require_sharing<'a>(
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

fn parse_share_id(value: &str, request_id: &RequestId) -> Result<ShareLinkId, ApiError> {
    value.parse().map_err(|_| {
        ApiError::bad_request(request_id.clone(), "INVALID_SHARE_ID", "Invalid share ID")
    })
}

fn parse_embed_id(value: &str, request_id: &RequestId) -> Result<EmbedLinkId, ApiError> {
    value.parse().map_err(|_| {
        ApiError::bad_request(request_id.clone(), "INVALID_EMBED_ID", "Invalid embed ID")
    })
}

fn share_not_found(request_id: RequestId) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "SHARE_NOT_FOUND",
        "Share link was not found",
        request_id,
    )
}

fn embed_not_found(request_id: RequestId) -> ApiError {
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
fn share_unavailable(request_id: RequestId) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "SHARE_UNAVAILABLE",
        "This link is not available",
        request_id,
    )
}

fn embed_unavailable(request_id: RequestId) -> ApiError {
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
fn target_error(error: ServiceError, request_id: RequestId) -> ApiError {
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

fn sharing_to_api_error(error: &SharingError, request_id: RequestId) -> ApiError {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_advertised_embeddable_types_are_exactly_the_ones_accepted() {
        for content_type in EMBEDDABLE_CONTENT_TYPES {
            let kind = PreviewKind::classify(Some(content_type));
            assert!(
                kind.allows_element_embed(),
                "{content_type} is advertised but not element-embeddable"
            );
            assert!(
                PreviewKind::canonical_content_type(content_type).is_some(),
                "{content_type} has no canonical form"
            );
        }
    }

    #[test]
    fn client_identity_prefers_a_forwarded_address_and_sanitises_it() {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.7, 10.0.0.1"),
        );
        assert_eq!(client_identity(&headers, None), "203.0.113.7");

        let mut hostile = header::HeaderMap::new();
        hostile.insert(
            "x-forwarded-for",
            HeaderValue::from_static("not an address at all"),
        );
        assert_eq!(client_identity(&hostile, None), "unknown");

        let mut oversized = header::HeaderMap::new();
        oversized.insert(
            "x-forwarded-for",
            HeaderValue::from_str(&"1".repeat(200)).expect("header"),
        );
        assert_eq!(client_identity(&oversized, None), "unknown");

        assert_eq!(client_identity(&header::HeaderMap::new(), None), "unknown");
    }

    #[test]
    fn every_unusable_share_state_produces_one_indistinguishable_answer() {
        let request_id = RequestId::new();
        let response = share_unavailable(request_id).into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn content_policies_deny_script_and_keep_stored_bytes_in_an_opaque_origin() {
        for policy in [SHARE_CONTENT_POLICY, EMBED_CONTENT_POLICY] {
            assert!(policy.contains("sandbox"), "{policy}");
            assert!(policy.contains("default-src 'none'"), "{policy}");
            assert!(!policy.contains("allow-scripts"), "{policy}");
            assert!(!policy.contains("allow-same-origin"), "{policy}");
            assert!(!policy.contains("unsafe-inline"), "{policy}");
        }
        // Only the share and preview surface is framed by Record Store itself.
        assert!(SHARE_CONTENT_POLICY.contains("frame-ancestors 'self'"));
    }
}
