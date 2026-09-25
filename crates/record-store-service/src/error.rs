//! Shared bucket and object application services.

use record_store_core::{CoreError, LockBlock, LockChangeRefused, VersionId};
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
    /// Object Lock still holds the version the caller wanted to remove.
    #[error("object version is held by {}", .0.label())]
    ObjectLocked(LockBlock),
    /// The requested lock change would have released a protected version.
    #[error("object lock change refused: {}", .0.label())]
    ObjectLockChangeRefused(LockChangeRefused),
    /// A governance bypass was presented but could not be recorded.
    ///
    /// Refused rather than performed: a bypass is the one way a retained
    /// version leaves before its date, and one nobody can account for
    /// afterwards is worse than one that did not happen.
    #[error("a governance bypass cannot be exercised without a durable audit record")]
    BypassNotRecordable,
    /// The bucket does not have Object Lock enabled.
    #[error("object lock is not enabled on this bucket")]
    ObjectLockNotEnabled,
    /// The bucket has Object Lock enabled but no configuration to report.
    #[error("object lock configuration was not found")]
    ObjectLockConfigurationNotFound,
    /// Object Lock requires version history, so versioning cannot be suspended.
    #[error("bucket versioning cannot be suspended while object lock is enabled")]
    ObjectLockRequiresVersioning,
    /// Wall-clock time is behind the recorded high-water mark, so no retention
    /// decision that would release an object can be trusted right now.
    #[error(
        "retention cannot be evaluated: the system clock is behind the recorded high-water mark"
    )]
    RetentionClockUnavailable,
    /// Custom metadata exceeded a configured bound.
    #[error("custom metadata exceeds configured limits")]
    MetadataTooLarge,
    /// Request parameters are invalid.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// Metadata repository failure.
    #[error("metadata operation failed: {0}")]
    Metadata(#[from] MetadataError),
    /// Stored bytes did not match what was committed for them.
    ///
    /// Kept apart from a generic storage failure because it is not a transient
    /// condition a caller should retry: the durable bytes are wrong, and the
    /// operator needs to see that rather than a 500 that looks like a blip.
    #[error("stored object failed integrity verification")]
    IntegrityMismatch,
    /// Storage engine failure.
    #[error("storage operation failed: {0}")]
    Storage(#[from] StorageError),
    /// Fine-grained coordination state was poisoned.
    #[error("operation coordination failed")]
    Coordination,
    /// Backpressure subsystem is unavailable.
    #[error("service is unavailable")]
    Unavailable,
    /// Too much work is already in flight for this operation to start.
    ///
    /// Distinct from [`ServiceError::Unavailable`] because the answers differ:
    /// this one is the deployment working as configured, and the caller should
    /// back off and retry rather than treat it as a fault.
    #[error("too many operations are already in flight")]
    Overloaded,
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
        MetadataError::VersionLocked(block) => ServiceError::ObjectLocked(block),
        MetadataError::ObjectLockChangeRefused(reason) => {
            ServiceError::ObjectLockChangeRefused(reason)
        }
        MetadataError::ObjectLockNotEnabled => ServiceError::ObjectLockNotEnabled,
        MetadataError::ObjectLockVersionNotFound => ServiceError::ObjectNotFound,
        MetadataError::ObjectLockRequiresVersioning
        | MetadataError::ObjectLockNotEnabledAtCreation => {
            ServiceError::ObjectLockRequiresVersioning
        }
        MetadataError::ClockWentBackwards => ServiceError::RetentionClockUnavailable,
        MetadataError::InvalidObjectLock(reason) => ServiceError::InvalidRequest(reason),
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
        // Object Lock is enforced inside the metadata transaction, so on the
        // delete path it surfaces wrapped in a storage error. Left unmapped it
        // would reach the client as a 500, telling them to retry something that
        // is meant never to succeed.
        StorageError::Metadata(error @ MetadataError::VersionLocked(_))
        | StorageError::Metadata(error @ MetadataError::ClockWentBackwards)
        | StorageError::Metadata(error @ MetadataError::ObjectLockChangeRefused(_))
        | StorageError::Metadata(error @ MetadataError::ObjectLockNotEnabled)
        | StorageError::Metadata(error @ MetadataError::ObjectLockVersionNotFound)
        | StorageError::Metadata(error @ MetadataError::ObjectLockRequiresVersioning)
        | StorageError::Metadata(error @ MetadataError::ObjectLockNotEnabledAtCreation)
        | StorageError::Metadata(error @ MetadataError::InvalidObjectLock(_)) => {
            map_metadata(error)
        }
        StorageError::IntegrityMismatch => ServiceError::IntegrityMismatch,
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
            map_storage(StorageError::Coordination),
            ServiceError::Storage(StorageError::Coordination)
        ));
    }

    /// Corrupt stored bytes are not a generic backend failure. The category
    /// has to survive the mapping or every protocol reports them as a retryable
    /// internal error, which is the opposite of what an operator needs to see.
    #[test]
    fn an_integrity_failure_keeps_its_own_category() {
        assert!(matches!(
            map_storage(StorageError::IntegrityMismatch),
            ServiceError::IntegrityMismatch
        ));
    }
}
