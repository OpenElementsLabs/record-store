//! Streaming object storage boundary and local filesystem implementation.

use std::{collections::BTreeMap, io, pin::Pin};

use bytes::Bytes;
use futures_core::Stream;
use record_store_core::{
    BucketId, ByteRange, Checksum, ETag, MultipartUpload, ObjectId, ObjectKey, ObjectMetadata,
    PartNumber, ResolvedByteRange, UploadId, UploadedPart, VersionId,
};

use crate::*;

/// A fallible, backpressure-aware incoming payload stream.
pub type UploadStream = Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send + 'static>>;

/// A fallible, backpressure-aware outgoing payload stream.
pub type DownloadStream = Pin<Box<dyn Stream<Item = Result<Bytes, StorageError>> + Send + 'static>>;

/// Creates a boxed upload stream from a compatible stream implementation.
pub fn upload_stream<S>(stream: S) -> UploadStream
where
    S: Stream<Item = Result<Bytes, io::Error>> + Send + 'static,
{
    Box::pin(stream)
}

/// Parameters for committing a streamed object.
pub struct PutObjectRequest {
    /// Destination bucket.
    pub bucket_id: BucketId,
    /// Logical object key.
    pub key: ObjectKey,
    /// Optional media type.
    pub content_type: Option<String>,
    /// Caller-supplied metadata.
    pub custom_metadata: BTreeMap<String, String>,
    /// Optional checksum supplied by the caller for end-to-end validation.
    pub expected_checksum: Option<Checksum>,
    /// Preallocated payload identifier used by crash-recoverable multipart completion.
    pub object_id: Option<ObjectId>,
    /// Protocol ETag override kept independent from the strong checksum.
    pub protocol_etag: Option<ETag>,
    /// Incoming payload chunks.
    pub body: UploadStream,
}

/// Parameters for durably streaming one multipart part.
pub struct PutMultipartPartRequest {
    /// Opaque upload identifier.
    pub upload_id: UploadId,
    /// Validated one-based part number.
    pub number: PartNumber,
    /// Optional strong checksum supplied by the client.
    pub expected_checksum: Option<Checksum>,
    /// Incoming payload chunks.
    pub body: UploadStream,
}

/// Parameters for combining an already validated multipart manifest.
#[derive(Debug, Clone)]
pub struct CompleteMultipartRequest {
    /// Durable upload descriptor.
    pub upload: MultipartUpload,
    /// Ordered durable parts matching the client manifest.
    pub parts: Vec<UploadedPart>,
}

/// Result of a committed object upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutObjectResult {
    /// Metadata made visible by the commit.
    pub metadata: ObjectMetadata,
}

/// Parameters for opening a streamed object read.
#[derive(Debug, Clone)]
pub struct GetObjectRequest {
    /// Source bucket.
    pub bucket_id: BucketId,
    /// Logical object key.
    pub key: ObjectKey,
    /// Optional byte range. The tail is clamped to the payload length.
    pub range: Option<ByteRange>,
}

/// Parameters for opening one immutable historical version.
#[derive(Debug, Clone)]
pub struct GetObjectVersionRequest {
    /// Source bucket.
    pub bucket_id: BucketId,
    /// Logical key used to prevent cross-key identifier confusion.
    pub key: ObjectKey,
    /// Stable requested version.
    pub version_id: VersionId,
    /// Optional byte range.
    pub range: Option<ByteRange>,
}

/// Result of opening an object read.
pub struct GetObjectResult {
    /// Committed object metadata.
    pub metadata: ObjectMetadata,
    /// Resolved range when a partial read was requested.
    pub range: Option<ResolvedByteRange>,
    /// Payload chunks read lazily with backpressure.
    pub body: DownloadStream,
}

/// Parameters for metadata-only object lookup.
#[derive(Debug, Clone)]
pub struct HeadObjectRequest {
    /// Source bucket.
    pub bucket_id: BucketId,
    /// Logical object key.
    pub key: ObjectKey,
}

/// Parameters for deleting the visible version of an object.
#[derive(Debug, Clone)]
pub struct DeleteObjectRequest {
    /// Source bucket.
    pub bucket_id: BucketId,
    /// Logical object key.
    pub key: ObjectKey,
}

/// Parameters for permanently deleting one explicit version.
#[derive(Debug, Clone)]
pub struct DeleteObjectVersionRequest {
    /// Source bucket.
    pub bucket_id: BucketId,
    /// Logical key.
    pub key: ObjectKey,
    /// Stable version identifier.
    pub version_id: VersionId,
}

/// Parameters for an explicit on-demand integrity verification.
#[derive(Debug, Clone)]
pub struct VerifyObjectRequest {
    /// Source bucket.
    pub bucket_id: BucketId,
    /// Logical object key.
    pub key: ObjectKey,
}

/// Cheap local filesystem capacity and temporary-state measurements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageStatus {
    /// Total filesystem capacity containing the data directory.
    pub capacity_bytes: u64,
    /// Currently available bytes on that filesystem.
    pub available_bytes: u64,
    /// Bytes held by recognized incomplete upload files.
    pub temporary_upload_bytes: u64,
}

/// Bounded consistency inspection without exposing filesystem paths.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct StorageInspection {
    pub metadata_payloads_scanned: u64,
    pub data_payloads_scanned: u64,
    pub metadata_without_data: u64,
    pub data_without_metadata: u64,
    pub unknown_data_entries: u64,
    pub recognized_temporary_entries: u64,
    pub unknown_temporary_entries: u64,
    pub truncated: bool,
    pub missing_payload_samples: Vec<ObjectId>,
    pub orphan_payload_samples: Vec<ObjectId>,
}

/// Explicit dry-run-by-default repair input.
#[derive(Debug, Clone, Copy)]
pub struct StorageRepairRequest {
    pub maximum_entries: usize,
    pub dry_run: bool,
}

/// Repair outcome. Suspicious unknown files are never removed automatically.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct StorageRepairResult {
    pub inspection: StorageInspection,
    pub removed_orphan_payloads: u64,
    pub dry_run: bool,
}
