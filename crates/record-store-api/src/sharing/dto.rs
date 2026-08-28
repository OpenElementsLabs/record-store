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
    extract::{Extension, State},
};
use chrono::{DateTime, Utc};
use record_store_core::{EmbedLinkId, ShareLinkId, VersionId};
use record_store_sharing::{
    CapabilityStatus, EmbedDisposition, EmbedLink, ShareLink, SharePermission,
};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::dto::RequestId;
use crate::error::ApiError;

use crate::sharing::support::require_sharing;

/// What a deployment permits, so the console offers only what will be accepted.
#[derive(Debug, Serialize)]
pub(crate) struct SharingSettingsResponse {
    pub(crate) shares_enabled: bool,
    pub(crate) embeds_enabled: bool,
    pub(crate) maximum_lifetime_days: Option<i64>,
    pub(crate) require_expiration: bool,
    pub(crate) require_share_password: bool,
    pub(crate) maximum_access_count: u32,
    pub(crate) minimum_password_length: usize,
    pub(crate) preview_text_limit_bytes: u64,
    /// Element embeds Record Store will accept, so the console can explain a refusal
    /// before the operator fills in a dialog.
    pub(crate) embeddable_content_types: Vec<&'static str>,
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
pub(crate) const EMBEDDABLE_CONTENT_TYPES: &[&str] = &[
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
    pub(crate) id: ShareLinkId,
    pub(crate) label: String,
    pub(crate) bucket: String,
    pub(crate) key: String,
    pub(crate) version_mode: &'static str,
    pub(crate) version_id: Option<VersionId>,
    pub(crate) permission: SharePermission,
    pub(crate) status: CapabilityStatus,
    pub(crate) password_protected: bool,
    pub(crate) created_by: String,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) expires_at: Option<DateTime<Utc>>,
    pub(crate) revoked_at: Option<DateTime<Utc>>,
    pub(crate) last_accessed_at: Option<DateTime<Utc>>,
    pub(crate) access_count: u32,
    pub(crate) maximum_access_count: Option<u32>,
}

impl ShareResponse {
    pub(crate) fn of(link: &ShareLink, now: DateTime<Utc>) -> Self {
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
    pub(crate) id: EmbedLinkId,
    pub(crate) label: String,
    pub(crate) bucket: String,
    pub(crate) key: String,
    pub(crate) version_mode: &'static str,
    pub(crate) version_id: Option<VersionId>,
    pub(crate) status: CapabilityStatus,
    pub(crate) content_type: String,
    pub(crate) disposition: EmbedDisposition,
    pub(crate) allowed_origins: Vec<String>,
    pub(crate) created_by: String,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) updated_at: Option<DateTime<Utc>>,
    pub(crate) expires_at: Option<DateTime<Utc>>,
    pub(crate) revoked_at: Option<DateTime<Utc>>,
    pub(crate) last_accessed_at: Option<DateTime<Utc>>,
    pub(crate) access_count: u64,
}

impl EmbedResponse {
    pub(crate) fn of(link: &EmbedLink, now: DateTime<Utc>) -> Self {
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
    pub(crate) share: ShareResponse,
    pub(crate) url: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct IssuedEmbedResponse {
    pub(crate) embed: EmbedResponse,
    pub(crate) url: String,
}

/// The URL of an existing capability, fetched only when it is about to be used.
///
/// Split out of the listing on purpose. A list of shares is read often, by
/// anything from a dashboard to a log-capturing proxy, and putting live tokens
/// in that response would scatter working capabilities across places nobody
/// audited. Copying a link is a deliberate act, so it gets a deliberate request.
#[derive(Debug, Serialize)]
pub(crate) struct CapabilityUrlResponse {
    pub(crate) url: Option<String>,
    /// False when the stored token can no longer be decrypted, which happens
    /// after the deployment's master key changes. The capability still works;
    /// only redisplaying it does not.
    pub(crate) available: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateShareBody {
    pub(crate) label: String,
    #[serde(default)]
    pub(crate) version_id: Option<VersionId>,
    #[serde(default = "default_permission")]
    pub(crate) permission: SharePermission,
    #[serde(default)]
    pub(crate) expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub(crate) password: Option<String>,
    #[serde(default)]
    pub(crate) maximum_access_count: Option<u32>,
}

pub(crate) const fn default_permission() -> SharePermission {
    SharePermission::ViewAndDownload
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateEmbedBody {
    pub(crate) label: String,
    #[serde(default)]
    pub(crate) version_id: Option<VersionId>,
    #[serde(default)]
    pub(crate) expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub(crate) allowed_origins: Vec<String>,
    #[serde(default)]
    pub(crate) disposition: EmbedDisposition,
}

#[derive(Debug, Deserialize)]
pub(crate) struct UpdateEmbedBody {
    pub(crate) allowed_origins: Vec<String>,
}

///
/// Everything Record Store knows and does not need to say is left out: the bucket, the
/// key's path, the version identifier, the checksum, the storage layout. A
/// recipient needs to recognise the file and decide whether to open it, and
/// that is the whole list.
#[derive(Debug, Serialize)]
pub(crate) struct PublicShareResponse {
    pub(crate) state: &'static str,
    pub(crate) file_name: Option<String>,
    pub(crate) content_type: Option<String>,
    pub(crate) size: Option<u64>,
    pub(crate) preview: Option<&'static str>,
    pub(crate) can_view: bool,
    pub(crate) can_download: bool,
    pub(crate) expires_at: Option<DateTime<Utc>>,
    /// How much of a text object the viewer should read before saying so.
    pub(crate) preview_text_limit_bytes: u64,
}

impl PublicShareResponse {
    pub(crate) fn locked() -> Self {
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
    pub(crate) password: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct UnlockResponse {
    pub(crate) ticket: String,
    pub(crate) expires_in_seconds: i64,
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct ShareContentQuery {
    /// Present when the recipient asked to save the file rather than view it.
    #[serde(default)]
    pub(crate) download: bool,
}
