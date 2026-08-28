//! Durable single-node metadata catalog.

use record_store_core::{
    BucketId, DeleteMarker, MultipartUpload, ObjectId, ObjectMetadata, ObjectVersionRecord,
    UploadId, UploadedPart, VersionId,
};
use serde::{Deserialize, Serialize};

/// Bounded ordered object-listing input.
#[derive(Debug, Clone)]
pub struct ListObjectsRequest {
    pub bucket_id: BucketId,
    pub prefix: String,
    pub start_after: Option<String>,
    pub limit: usize,
}

/// Bounded ordered current-object page.
#[derive(Debug, Clone)]
pub struct ObjectMetadataPage {
    pub objects: Vec<ObjectMetadata>,
    pub next_key: Option<String>,
}

/// Bounded ordered version-listing input.
#[derive(Debug, Clone)]
pub struct ListObjectVersionsRequest {
    pub bucket_id: BucketId,
    pub prefix: String,
    pub key_marker: Option<String>,
    pub version_id_marker: Option<VersionId>,
    pub limit: usize,
}

/// Version entry with current-state annotation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedObjectVersion {
    pub record: ObjectVersionRecord,
    pub is_latest: bool,
}

/// Bounded ordered object-version page.
#[derive(Debug, Clone)]
pub struct ObjectVersionPage {
    pub versions: Vec<ListedObjectVersion>,
    pub next_key_marker: Option<String>,
    pub next_version_id_marker: Option<VersionId>,
}

/// Bounded multipart-upload listing input.
#[derive(Debug, Clone)]
pub struct ListMultipartUploadsRequest {
    pub bucket_id: BucketId,
    pub prefix: String,
    pub upload_id_marker: Option<UploadId>,
    pub limit: usize,
}

/// Bounded multipart-upload page.
#[derive(Debug, Clone)]
pub struct MultipartUploadPage {
    pub uploads: Vec<MultipartUpload>,
    pub next_upload_id_marker: Option<UploadId>,
}

/// Publication result naming only payloads that became unreachable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectCommitResult {
    pub cleanup: Vec<ObjectMetadata>,
}

/// Result of applying an ordinary delete.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteObjectResult {
    pub delete_marker: Option<DeleteMarker>,
    pub cleanup: Vec<ObjectMetadata>,
    pub previously_visible: bool,
}

/// Result of permanently deleting an explicit version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteVersionResult {
    pub removed: ObjectVersionRecord,
    pub cleanup: Option<ObjectMetadata>,
}

/// Multipart state removed after completion or abort.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultipartCleanupResult {
    pub parts: Vec<UploadedPart>,
}

/// Bounded page of immutable payload identifiers referenced by metadata.
#[derive(Debug, Clone, Default)]
pub struct PayloadReferencePage {
    pub object_ids: Vec<ObjectId>,
    pub next_object_id: Option<ObjectId>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BucketUsage {
    pub(crate) current_objects: u64,
    pub(crate) logical_bytes: u64,
    pub(crate) versions: u64,
    pub(crate) version_bytes: u64,
    pub(crate) multipart_bytes: u64,
}

/// Per-bucket accounting maintained transactionally with every commit.
///
/// These counters exist so that listing buckets can report size and object
/// counts in one request instead of one request per bucket.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketUsageSummary {
    /// Currently visible objects.
    pub object_count: u64,
    /// Bytes referenced by currently visible objects.
    pub logical_bytes: u64,
    /// Immutable versions, including current versions.
    pub version_count: u64,
    /// Bytes referenced by every immutable version.
    pub version_bytes: u64,
    /// Bytes held by durable multipart parts.
    pub multipart_bytes: u64,
}

impl From<BucketUsage> for BucketUsageSummary {
    fn from(value: BucketUsage) -> Self {
        Self {
            object_count: value.current_objects,
            logical_bytes: value.logical_bytes,
            version_count: value.versions,
            version_bytes: value.version_bytes,
            multipart_bytes: value.multipart_bytes,
        }
    }
}
