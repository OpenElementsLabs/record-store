use std::fmt::{self, Display, Formatter};

use axum::http::header;
use record_store_config::DeploymentMode;
use record_store_core::{Bucket, ClusterId, ObjectMetadata, VersionId};
use record_store_storage::StorageStatus;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;
use crate::handlers::audit::default_audit_limit;
use crate::*;

/// A validated request correlation identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestId(pub(crate) String);

impl RequestId {
    pub(crate) fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    pub(crate) fn accept(value: &str) -> Option<Self> {
        (!value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte)))
        .then(|| Self(value.to_owned()))
    }

    /// Returns the validated identifier text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for RequestId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct StatusResponse {
    pub(crate) status: &'static str,
}

#[derive(Debug, Serialize)]
pub(crate) struct SystemInfoResponse {
    pub(crate) name: &'static str,
    pub(crate) version: &'static str,
    pub(crate) status: &'static str,
    pub(crate) mode: DeploymentMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cluster_id: Option<ClusterId>,
    pub(crate) capabilities: Capabilities,
}

/// What this deployment can actually do.
///
/// Clients use this instead of inferring behaviour from a version number, so a
/// build that lacks a capability simply reports it as unavailable.
#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) struct Capabilities {
    /// Cluster membership, replication, repair, and rebalancing are available.
    pub(crate) cluster: bool,
    /// Bucket versioning can be enabled and versions can be listed.
    pub(crate) versioning: bool,
    /// Signed outbound storage-event webhooks are available.
    pub(crate) webhooks: bool,
    /// Storage-event history can be queried.
    pub(crate) events: bool,
    /// Metadata-driven object expiration is available.
    pub(crate) lifecycle: bool,
    /// Object bytes can be browsed and transferred through this API.
    pub(crate) object_browser: bool,
    /// Erasure coding is not implemented; replication is the durability model.
    pub(crate) erasure_coding: bool,
}

impl Capabilities {
    pub(crate) fn detect(state: &AppState) -> Self {
        Self {
            cluster: state.cluster.is_some(),
            versioning: true,
            webhooks: state.events.is_some(),
            events: state.events.is_some(),
            lifecycle: true,
            object_browser: true,
            erasure_coding: false,
        }
    }
}

/// A bucket with the accounting a console needs to render a table.
///
/// Usage is included here so listing buckets costs one request rather than one
/// request per bucket.
#[derive(Debug, Serialize)]
pub(crate) struct BucketSummary {
    #[serde(flatten)]
    pub(crate) bucket: Bucket,
    pub(crate) object_count: u64,
    pub(crate) logical_bytes: u64,
    pub(crate) version_count: u64,
    pub(crate) version_bytes: u64,
    pub(crate) multipart_bytes: u64,
}

/// Object metadata safe to expose to a management client.
///
/// Internal payload identifiers and physical representation are deliberately
/// omitted: where an object physically lives is not a client concern.
#[derive(Debug, Serialize)]
pub(crate) struct ObjectSummary {
    pub(crate) key: String,
    pub(crate) size: u64,
    pub(crate) content_type: Option<String>,
    pub(crate) etag: String,
    pub(crate) checksum: String,
    pub(crate) version_id: VersionId,
    pub(crate) created_at: chrono::DateTime<chrono::Utc>,
    pub(crate) modified_at: chrono::DateTime<chrono::Utc>,
    pub(crate) custom_metadata: std::collections::BTreeMap<String, String>,
}

impl From<ObjectMetadata> for ObjectSummary {
    fn from(value: ObjectMetadata) -> Self {
        Self {
            key: value.key.to_string(),
            size: value.size,
            content_type: value.content_type,
            etag: value.etag.as_str().to_owned(),
            checksum: value.checksum.to_string(),
            version_id: value.version_id,
            created_at: value.created_at,
            modified_at: value.modified_at,
            custom_metadata: value.custom_metadata,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ObjectListQuery {
    #[serde(default)]
    pub(crate) prefix: String,
    pub(crate) delimiter: Option<String>,
    pub(crate) continuation_token: Option<String>,
    #[serde(default = "default_object_limit")]
    pub(crate) limit: usize,
}

pub(crate) const fn default_object_limit() -> usize {
    100
}

/// One page of a prefix listing.
///
/// Prefixes are logical groupings derived from the delimiter; Record Store stores no
/// directories, so they are reported separately from objects rather than being
/// presented as entries of the same kind.
#[derive(Debug, Serialize)]
pub(crate) struct ObjectListResponse {
    pub(crate) objects: Vec<ObjectSummary>,
    pub(crate) prefixes: Vec<String>,
    pub(crate) is_truncated: bool,
    pub(crate) next_continuation_token: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ObjectVersionListQuery {
    #[serde(default)]
    pub(crate) prefix: String,
    pub(crate) key_marker: Option<String>,
    pub(crate) version_id_marker: Option<VersionId>,
    #[serde(default = "default_object_limit")]
    pub(crate) limit: usize,
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct PreviewQuery {
    pub(crate) version_id: Option<VersionId>,
}

pub(crate) fn parse_preview_range(
    headers: &header::HeaderMap,
    size: u64,
    request_id: &RequestId,
) -> Result<Option<record_store_core::ByteRange>, ApiError> {
    let Some(value) = headers.get(header::RANGE) else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_RANGE",
            "Range must be an ASCII byte range",
        )
    })?;
    let Some(value) = value.strip_prefix("bytes=") else {
        return Err(ApiError::bad_request(
            request_id.clone(),
            "INVALID_RANGE",
            "Only byte ranges are supported",
        ));
    };
    if value.contains(',') {
        return Err(ApiError::bad_request(
            request_id.clone(),
            "INVALID_RANGE",
            "Only one byte range is supported",
        ));
    }
    let (start, end) = value.split_once('-').ok_or_else(|| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_RANGE",
            "Range must use bytes=start-end syntax",
        )
    })?;
    let (offset, length) = if start.is_empty() {
        let suffix = end.parse::<u64>().map_err(|_| {
            ApiError::bad_request(request_id.clone(), "INVALID_RANGE", "Range is invalid")
        })?;
        (size.saturating_sub(suffix), suffix.min(size))
    } else {
        let offset = start.parse::<u64>().map_err(|_| {
            ApiError::bad_request(request_id.clone(), "INVALID_RANGE", "Range is invalid")
        })?;
        let end = if end.is_empty() {
            size
        } else {
            end.parse::<u64>()
                .map_err(|_| {
                    ApiError::bad_request(request_id.clone(), "INVALID_RANGE", "Range is invalid")
                })?
                .checked_add(1)
                .ok_or_else(|| {
                    ApiError::bad_request(request_id.clone(), "INVALID_RANGE", "Range is invalid")
                })?
        };
        (
            offset,
            end.saturating_sub(offset).min(size.saturating_sub(offset)),
        )
    };
    record_store_core::ByteRange::new(offset, length)
        .map(Some)
        .map_err(|_| ApiError::bad_request(request_id.clone(), "INVALID_RANGE", "Range is invalid"))
}

/// One version-history entry.
#[derive(Debug, Serialize)]
pub(crate) struct ObjectVersionEntry {
    pub(crate) key: String,
    pub(crate) version_id: VersionId,
    pub(crate) is_latest: bool,
    pub(crate) is_delete_marker: bool,
    /// Whether S3 exposes this entry as the special unversioned entry.
    pub(crate) is_null: bool,
    pub(crate) created_at: chrono::DateTime<chrono::Utc>,
    pub(crate) size: Option<u64>,
    pub(crate) etag: Option<String>,
    pub(crate) checksum: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ObjectVersionListResponse {
    pub(crate) versions: Vec<ObjectVersionEntry>,
    pub(crate) next_key_marker: Option<String>,
    pub(crate) next_version_id_marker: Option<VersionId>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DeleteVersionQuery {
    pub(crate) version_id: VersionId,
}

#[derive(Debug, Deserialize)]
pub(crate) struct EventQueryParameters {
    pub(crate) since: Option<chrono::DateTime<chrono::Utc>>,
    pub(crate) until: Option<chrono::DateTime<chrono::Utc>>,
    pub(crate) bucket: Option<String>,
    #[serde(rename = "type")]
    pub(crate) event_type: Option<record_store_events::StorageEventType>,
    pub(crate) prefix: Option<String>,
    pub(crate) after_time: Option<chrono::DateTime<chrono::Utc>>,
    pub(crate) after_id: Option<record_store_core::EventId>,
    #[serde(default = "default_audit_limit")]
    pub(crate) limit: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct EventsResponse {
    pub(crate) events: Vec<record_store_events::StorageEvent>,
    pub(crate) next_time: Option<chrono::DateTime<chrono::Utc>>,
    pub(crate) next_id: Option<record_store_core::EventId>,
}

#[derive(Debug, Serialize)]
pub(crate) struct StorageStatusResponse {
    pub(crate) capacity_bytes: u64,
    pub(crate) available_bytes: u64,
    pub(crate) temporary_upload_bytes: u64,
}

impl From<StorageStatus> for StorageStatusResponse {
    fn from(value: StorageStatus) -> Self {
        Self {
            capacity_bytes: value.capacity_bytes,
            available_bytes: value.available_bytes,
            temporary_upload_bytes: value.temporary_upload_bytes,
        }
    }
}
