//! Shared bucket and object application services.

use std::collections::BTreeSet;

use record_store_core::{
    BucketName, Checksum, CompletedPart, MultipartUpload, ObjectKey, ObjectMetadata, PartNumber,
    UploadId, VersionId,
};
use record_store_metadata::ListedObjectVersion;
use record_store_storage::{DownloadStream, UploadStream};

/// Service-layer upload parameters.
pub struct ServicePutRequest {
    /// Destination bucket name.
    pub bucket: BucketName,
    /// Logical key.
    pub key: ObjectKey,
    /// Optional media type.
    pub content_type: Option<String>,
    /// Caller custom metadata.
    pub custom_metadata: std::collections::BTreeMap<String, String>,
    /// Optional expected SHA-256 checksum.
    pub expected_checksum: Option<record_store_core::Checksum>,
    /// Streaming body.
    pub body: UploadStream,
}

/// Multipart initiation parameters.
pub struct ServiceCreateMultipartRequest {
    pub bucket: BucketName,
    pub key: ObjectKey,
    pub content_type: Option<String>,
    pub custom_metadata: std::collections::BTreeMap<String, String>,
}

/// Streaming multipart-part parameters.
pub struct ServiceUploadPartRequest {
    pub bucket: BucketName,
    pub key: ObjectKey,
    pub upload_id: UploadId,
    pub number: PartNumber,
    pub expected_checksum: Option<Checksum>,
    pub body: UploadStream,
}

/// Multipart completion parameters.
#[derive(Debug, Clone)]
pub struct ServiceCompleteMultipartRequest {
    pub bucket: BucketName,
    pub key: ObjectKey,
    pub upload_id: UploadId,
    pub manifest: Vec<CompletedPart>,
}

/// Version-listing parameters.
#[derive(Debug, Clone)]
pub struct ServiceListVersionsRequest {
    pub bucket: BucketName,
    pub prefix: String,
    pub key_marker: Option<String>,
    pub version_id_marker: Option<VersionId>,
    pub maximum_keys: usize,
}

/// Version-listing result.
#[derive(Debug, Clone)]
pub struct ServiceListVersionsResult {
    pub versions: Vec<ListedObjectVersion>,
    pub next_key_marker: Option<String>,
    pub next_version_id_marker: Option<VersionId>,
}

/// Multipart-upload listing parameters.
#[derive(Debug, Clone)]
pub struct ServiceListMultipartUploadsRequest {
    pub bucket: BucketName,
    pub prefix: String,
    pub upload_id_marker: Option<UploadId>,
    pub maximum_uploads: usize,
}

/// Multipart-upload listing result.
#[derive(Debug, Clone)]
pub struct ServiceListMultipartUploadsResult {
    pub uploads: Vec<MultipartUpload>,
    pub next_upload_id_marker: Option<UploadId>,
}

/// S3 metadata behavior for server-side copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMetadataDirective {
    Copy,
    Replace,
}

/// Server-side streaming-copy parameters.
#[derive(Debug, Clone)]
pub struct ServiceCopyRequest {
    pub source_bucket: BucketName,
    pub source_key: ObjectKey,
    pub source_version_id: Option<VersionId>,
    pub destination_bucket: BucketName,
    pub destination_key: ObjectKey,
    pub metadata_directive: CopyMetadataDirective,
    pub content_type: Option<String>,
    pub replacement_metadata: std::collections::BTreeMap<String, String>,
}

/// Service-layer streaming read result.
pub struct ServiceGetResult {
    /// Persisted metadata.
    pub metadata: ObjectMetadata,
    /// Resolved range.
    pub range: Option<record_store_core::ResolvedByteRange>,
    /// Streaming payload.
    pub body: DownloadStream,
}

/// Detailed ordinary-delete outcome for S3 delete-marker headers.
#[derive(Debug, Clone)]
pub struct ServiceDeleteResult {
    pub delete_marker: Option<record_store_core::DeleteMarker>,
    pub previously_visible: bool,
}

/// Service-layer ordered listing parameters.
#[derive(Debug, Clone)]
pub struct ServiceListRequest {
    /// Bucket name.
    pub bucket: BucketName,
    /// Lexical prefix.
    pub prefix: String,
    /// Optional grouping delimiter.
    pub delimiter: Option<String>,
    /// Maximum combined object/common-prefix entries.
    pub maximum_keys: usize,
    /// Exclusive internal marker decoded from a continuation token.
    pub start_after: Option<String>,
}

/// Service-layer ordered listing output.
#[derive(Debug, Clone, Default)]
pub struct ServiceListResult {
    /// Visible object records.
    pub objects: Vec<ObjectMetadata>,
    /// Distinct common prefixes.
    pub common_prefixes: BTreeSet<String>,
    /// Whether another page exists.
    pub is_truncated: bool,
    /// Internal marker for the next page.
    pub next_marker: Option<String>,
}

impl ServiceListResult {
    pub(crate) fn entry_count(&self) -> usize {
        self.objects.len() + self.common_prefixes.len()
    }
}
