//! Shared bucket and object application services.

use record_store_core::{CoreError, VersionId};
use record_store_metadata::MetadataError;
use record_store_storage::StorageError;
use thiserror::Error;

/// Application-service failure categories mapped independently by each protocol.
#[derive(Debug, Error)]
pub enum ServiceError {
    /// Invalid domain input.
    #[error("invalid input: {0}")]
    Core(#[from] CoreError),
    /// Bucket is absent.
    #[error("bucket was not found")]
    BucketNotFound,
    /// Bucket name already exists.
    #[error("bucket already exists")]
    BucketAlreadyExists,
    /// Bucket contains committed objects.
    #[error("bucket is not empty")]
    BucketNotEmpty,
    /// Object is absent.
    #[error("object was not found")]
    ObjectNotFound,
    /// Requested version is a logical delete marker.
    #[error("object version is a delete marker: {0}")]
    DeleteMarker(VersionId),
    /// Multipart upload is absent or does not own the selected bucket/key.
    #[error("multipart upload was not found")]
    MultipartUploadNotFound,
    /// Multipart completion references a missing part or mismatched ETag.
    #[error("multipart completion contains an invalid part")]
    InvalidPart,
    /// Multipart completion parts were not strictly ascending.
    #[error("multipart completion parts are not in ascending order")]
    InvalidPartOrder,
    /// A non-final multipart part is below the S3 minimum size.
    #[error("multipart part is too small")]
    EntityTooSmall,
    /// Storage quota would be exceeded.
    #[error("storage quota exceeded")]
    QuotaExceeded,
    /// Custom metadata exceeded a configured bound.
    #[error("custom metadata exceeds configured limits")]
    MetadataTooLarge,
    /// Request parameters are invalid.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// Metadata repository failure.
    #[error("metadata operation failed: {0}")]
    Metadata(#[from] MetadataError),
    /// Storage engine failure.
    #[error("storage operation failed: {0}")]
    Storage(#[from] StorageError),
    /// Fine-grained coordination state was poisoned.
    #[error("operation coordination failed")]
    Coordination,
    /// Backpressure subsystem is unavailable.
    #[error("service is unavailable")]
    Unavailable,
    /// The cluster cannot currently satisfy the operation.
    ///
    /// This is reported honestly as a retryable condition rather than being
    /// hidden behind a generic internal error.
    #[error("cluster is unavailable for this operation: {0}")]
    ClusterUnavailable(String),
    /// The write could not reach its required durability.
    #[error("{0}")]
    DurabilityNotMet(String),
}

pub(crate) fn map_metadata(error: MetadataError) -> ServiceError {
    match error {
        MetadataError::BucketAlreadyExists => ServiceError::BucketAlreadyExists,
        MetadataError::BucketNotFound => ServiceError::BucketNotFound,
        MetadataError::BucketNotEmpty => ServiceError::BucketNotEmpty,
        MetadataError::MultipartUploadNotFound => ServiceError::MultipartUploadNotFound,
        MetadataError::QuotaExceeded => ServiceError::QuotaExceeded,
        error => ServiceError::Metadata(error),
    }
}

pub(crate) fn map_storage(error: StorageError) -> ServiceError {
    match error {
        StorageError::BucketNotFound => ServiceError::BucketNotFound,
        StorageError::ObjectNotFound => ServiceError::ObjectNotFound,
        StorageError::DeleteMarker { version_id } => ServiceError::DeleteMarker(version_id),
        StorageError::Metadata(MetadataError::MultipartUploadNotFound) => {
            ServiceError::MultipartUploadNotFound
        }
        StorageError::Metadata(MetadataError::QuotaExceeded) => ServiceError::QuotaExceeded,
        StorageError::ClusterUnavailable(reason) => ServiceError::ClusterUnavailable(reason),
        StorageError::NoHealthyReplica => {
            ServiceError::ClusterUnavailable(StorageError::NoHealthyReplica.to_string())
        }
        error @ StorageError::DurabilityNotMet { .. } => {
            ServiceError::DurabilityNotMet(error.to_string())
        }
        error => ServiceError::Storage(error),
    }
}
