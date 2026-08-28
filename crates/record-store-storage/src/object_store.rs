//! Streaming object storage boundary and local filesystem implementation.

use async_trait::async_trait;
use record_store_core::{ObjectMetadata, UploadedPart};
use record_store_metadata::DeleteObjectResult;

use crate::*;

/// Storage operations consumed by API and background components.
#[async_trait]
pub trait ObjectStore: Send + Sync {
    /// Streams and atomically commits an object.
    async fn put(&self, request: PutObjectRequest) -> Result<PutObjectResult, StorageError>;

    /// Streams and durably publishes one resumable multipart part.
    async fn put_multipart_part(
        &self,
        request: PutMultipartPartRequest,
    ) -> Result<UploadedPart, StorageError>;

    /// Streams durable parts into one atomically published logical object.
    async fn complete_multipart(
        &self,
        request: CompleteMultipartRequest,
    ) -> Result<PutObjectResult, StorageError>;

    /// Opens a lazy stream for an object or object range.
    async fn get(&self, request: GetObjectRequest) -> Result<GetObjectResult, StorageError>;

    /// Opens a lazy stream for one immutable object version.
    async fn get_version(
        &self,
        request: GetObjectVersionRequest,
    ) -> Result<GetObjectResult, StorageError>;

    /// Returns object metadata without opening its payload.
    async fn head(&self, request: HeadObjectRequest) -> Result<ObjectMetadata, StorageError>;

    /// Removes the visible object version.
    async fn delete(
        &self,
        request: DeleteObjectRequest,
    ) -> Result<DeleteObjectResult, StorageError>;

    /// Permanently removes one explicitly selected version.
    async fn delete_version(&self, request: DeleteObjectVersionRequest)
    -> Result<(), StorageError>;

    /// Recomputes and verifies the persisted integrity checksum on demand.
    async fn verify(&self, request: VerifyObjectRequest) -> Result<ObjectMetadata, StorageError>;

    /// Returns filesystem capacity and controlled temporary-upload usage.
    async fn status(&self) -> Result<StorageStatus, StorageError>;

    /// Verifies that required storage paths are writable.
    async fn check_ready(&self) -> Result<(), StorageError>;

    /// Inspects bounded metadata/data consistency without mutating state.
    async fn inspect(&self, maximum_entries: usize) -> Result<StorageInspection, StorageError>;

    /// Removes only positively identified unreferenced Record Store payloads when not a dry run.
    async fn repair(
        &self,
        request: StorageRepairRequest,
    ) -> Result<StorageRepairResult, StorageError>;

    /// Processes a bounded page of durable payload garbage.
    async fn cleanup_pending(&self, limit: usize) -> Result<usize, StorageError>;
}
