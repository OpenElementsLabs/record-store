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

#[cfg(test)]
mod tests {
    use record_store_core::VersionId;
    use record_store_metadata::MetadataError;
    use record_store_storage::StorageError;

    use super::*;

    /// A caller can only react to a failure it can name. Anything that has a
    /// dedicated service category must arrive as that category rather than as a
    /// generic backend error, because the protocol adapters map on the category
    /// alone and would otherwise turn a 404 into a 500.
    #[test]
    fn metadata_failures_a_caller_can_act_on_keep_their_own_category() {
        for (backend, expected) in [
            (
                MetadataError::BucketAlreadyExists,
                ServiceError::BucketAlreadyExists,
            ),
            (MetadataError::BucketNotFound, ServiceError::BucketNotFound),
            (MetadataError::BucketNotEmpty, ServiceError::BucketNotEmpty),
            (
                MetadataError::MultipartUploadNotFound,
                ServiceError::MultipartUploadNotFound,
            ),
            (MetadataError::QuotaExceeded, ServiceError::QuotaExceeded),
        ] {
            let rendered = format!("{backend:?}");
            assert_eq!(
                std::mem::discriminant(&map_metadata(backend)),
                std::mem::discriminant(&expected),
                "{rendered} was flattened instead of keeping its category"
            );
        }
    }

    #[test]
    fn unrecognised_metadata_failures_are_preserved_for_diagnosis() {
        let mapped = map_metadata(MetadataError::MultipartStateConflict);
        assert!(
            matches!(
                mapped,
                ServiceError::Metadata(MetadataError::MultipartStateConflict)
            ),
            "expected the original error to survive, got {mapped:?}"
        );
    }

    /// The storage layer wraps catalog failures, so the actionable ones are one
    /// level down. Unwrapping them is the difference between a client being told
    /// "that upload does not exist" and being told "something went wrong".
    #[test]
    fn storage_failures_keep_their_category_even_when_nested_in_metadata() {
        assert!(matches!(
            map_storage(StorageError::Metadata(
                MetadataError::MultipartUploadNotFound
            )),
            ServiceError::MultipartUploadNotFound
        ));
        assert!(matches!(
            map_storage(StorageError::Metadata(MetadataError::QuotaExceeded)),
            ServiceError::QuotaExceeded
        ));
    }

    #[test]
    fn a_delete_marker_carries_the_version_that_shadowed_the_object() {
        let version_id = VersionId::new();
        assert!(matches!(
            map_storage(StorageError::DeleteMarker { version_id }),
            ServiceError::DeleteMarker(carried) if carried == version_id
        ));
    }

    /// Both of these are transient cluster conditions. Reporting them as
    /// retryable is deliberate: a client that retries will often succeed, and a
    /// generic internal error would tell it to give up.
    #[test]
    fn transient_cluster_conditions_are_reported_as_retryable() {
        assert!(matches!(
            map_storage(StorageError::NoHealthyReplica),
            ServiceError::ClusterUnavailable(_)
        ));
        assert!(matches!(
            map_storage(StorageError::ClusterUnavailable("draining".into())),
            ServiceError::ClusterUnavailable(reason) if reason == "draining"
        ));
    }

    /// A durability shortfall must not be silently downgraded to a write that
    /// looks successful, and the operator-facing counts have to survive the
    /// translation or the message loses everything actionable in it.
    #[test]
    fn a_durability_shortfall_keeps_its_operator_facing_detail() {
        let mapped = map_storage(StorageError::DurabilityNotMet {
            required: 3,
            achieved: 1,
            detail: "node-b timed out".into(),
        });
        let ServiceError::DurabilityNotMet(message) = mapped else {
            panic!("durability shortfall lost its category");
        };
        assert!(message.contains('3'), "{message}");
        assert!(message.contains('1'), "{message}");
        assert!(message.contains("node-b timed out"), "{message}");
    }

    #[test]
    fn unrecognised_storage_failures_are_preserved_for_diagnosis() {
        assert!(matches!(
            map_storage(StorageError::IntegrityMismatch),
            ServiceError::Storage(StorageError::IntegrityMismatch)
        ));
    }
}
