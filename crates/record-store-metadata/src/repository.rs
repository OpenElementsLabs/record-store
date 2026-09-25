//! Durable single-node metadata catalog.

use async_trait::async_trait;
use record_store_core::{
    Bucket, BucketId, BucketName, BucketQuota, CorsConfiguration, LifecycleRule, LifecycleRuleId,
    MultipartUpload, MutationEvent, ObjectId, ObjectKey, ObjectLockConfiguration, ObjectLockState,
    ObjectMetadata, ObjectVersionRecord, PartNumber, StorageUsage, UploadId, UploadedPart,
    VersionId, VersioningState, WriteOrigin,
};

use crate::*;

#[async_trait]
pub trait MetadataRepository: Send + Sync {
    async fn create_bucket(&self, bucket: &Bucket) -> Result<(), MetadataError>;
    async fn get_bucket(&self, id: BucketId) -> Result<Option<Bucket>, MetadataError>;
    async fn get_bucket_by_name(&self, name: &BucketName) -> Result<Option<Bucket>, MetadataError>;
    async fn list_buckets(&self) -> Result<Vec<Bucket>, MetadataError>;
    async fn set_bucket_versioning(
        &self,
        id: BucketId,
        state: VersioningState,
    ) -> Result<Bucket, MetadataError>;
    async fn set_bucket_quota(
        &self,
        id: BucketId,
        quota: BucketQuota,
    ) -> Result<Bucket, MetadataError>;
    async fn set_bucket_cors(
        &self,
        id: BucketId,
        configuration: Option<CorsConfiguration>,
    ) -> Result<Bucket, MetadataError>;
    /// Replaces the default retention of a bucket that already has Object Lock
    /// enabled. Lock cannot be turned on here: that happens only at creation.
    async fn set_bucket_object_lock(
        &self,
        id: BucketId,
        configuration: ObjectLockConfiguration,
    ) -> Result<Bucket, MetadataError>;
    async fn delete_bucket(&self, name: &BucketName) -> Result<Bucket, MetadataError>;
    /// Publishes a version together with the Object Lock state it is born
    /// with, in one transaction, so a crash cannot durably lose the retention a
    /// write was accepted under.
    /// Publishes a version, with the reason it is being written.
    ///
    /// The origin is not decoration: the event a subscriber receives is derived
    /// from it inside the committing transaction, and a copy, a restore, and a
    /// completed multipart upload are otherwise indistinguishable there.
    async fn put_object(
        &self,
        metadata: &ObjectMetadata,
        object_lock: Option<ObjectLockState>,
        origin: WriteOrigin,
    ) -> Result<ObjectCommitResult, MetadataError>;
    /// Returns journalled storage events after `after`, in commit order.
    ///
    /// These are the events committed mutations still owe. They stay until an
    /// outbox has taken them, so this is what a restart resumes from.
    async fn pending_mutation_events(
        &self,
        after: u64,
        limit: usize,
    ) -> Result<Vec<MutationEvent>, MetadataError>;
    /// Removes journalled events an outbox has taken, up to and including
    /// `through_sequence`.
    async fn prune_mutation_events(&self, through_sequence: u64) -> Result<(), MetadataError>;
    async fn get_object(
        &self,
        bucket: BucketId,
        key: &ObjectKey,
    ) -> Result<Option<ObjectMetadata>, MetadataError>;
    async fn get_object_version(
        &self,
        bucket: BucketId,
        key: &ObjectKey,
        version: VersionId,
    ) -> Result<Option<ObjectVersionRecord>, MetadataError>;
    async fn get_null_version(
        &self,
        bucket: BucketId,
        key: &ObjectKey,
    ) -> Result<Option<ObjectVersionRecord>, MetadataError>;
    /// Applies ordinary delete semantics.
    ///
    /// The caller supplies the delete-marker identity so that the operation is
    /// deterministic and can be replicated through consensus.
    async fn delete_object(
        &self,
        bucket: BucketId,
        key: &ObjectKey,
        marker: NewDeleteMarker,
    ) -> Result<DeleteObjectResult, MetadataError>;
    /// Permanently removes one version, refusing while Object Lock holds it.
    async fn delete_object_version(
        &self,
        bucket: BucketId,
        key: &ObjectKey,
        version: VersionId,
        release: LockRelease,
    ) -> Result<Option<DeleteVersionResult>, MetadataError>;
    /// Returns the Object Lock state of one version. An unlocked version and a
    /// version that was never locked are the same answer.
    async fn get_object_lock(&self, version: VersionId) -> Result<ObjectLockState, MetadataError>;
    /// Replaces the Object Lock state of one version, enforcing the mode rules.
    async fn put_object_lock(
        &self,
        bucket: BucketId,
        key: &ObjectKey,
        version: VersionId,
        requested: ObjectLockState,
        release: LockRelease,
    ) -> Result<ObjectLockState, MetadataError>;
    /// Advances the observed-time high-water mark retention is judged against.
    async fn observe_clock(&self, release: LockRelease) -> Result<(), MetadataError>;
    /// Returns a bounded page of the versions Object Lock holds a record for.
    ///
    /// This scans the lock table rather than every version, because that table
    /// contains only locked versions and is therefore already the right index.
    /// A deployment with a million objects and ten locks pays for ten.
    async fn list_object_locks(
        &self,
        after: Option<VersionId>,
        limit: usize,
    ) -> Result<LockedVersionPage, MetadataError>;
    async fn list_objects(
        &self,
        request: ListObjectsRequest,
    ) -> Result<ObjectMetadataPage, MetadataError>;
    async fn list_object_versions(
        &self,
        request: ListObjectVersionsRequest,
    ) -> Result<ObjectVersionPage, MetadataError>;
    async fn create_multipart_upload(&self, upload: &MultipartUpload) -> Result<(), MetadataError>;
    async fn get_multipart_upload(
        &self,
        id: UploadId,
    ) -> Result<Option<MultipartUpload>, MetadataError>;
    async fn put_multipart_part(
        &self,
        part: &UploadedPart,
    ) -> Result<Option<UploadedPart>, MetadataError>;
    async fn list_multipart_parts(
        &self,
        id: UploadId,
        after: Option<PartNumber>,
        limit: usize,
    ) -> Result<Vec<UploadedPart>, MetadataError>;
    async fn list_multipart_uploads(
        &self,
        request: ListMultipartUploadsRequest,
    ) -> Result<MultipartUploadPage, MetadataError>;
    async fn begin_multipart_completion(
        &self,
        id: UploadId,
        object_id: ObjectId,
    ) -> Result<MultipartUpload, MetadataError>;
    async fn finish_multipart_upload(
        &self,
        id: UploadId,
    ) -> Result<MultipartCleanupResult, MetadataError>;
    async fn abort_multipart_upload(
        &self,
        id: UploadId,
    ) -> Result<MultipartCleanupResult, MetadataError>;
    /// Reconciles crash-interrupted completion state before readiness.
    async fn recover_multipart_completions(&self) -> Result<MultipartCleanupResult, MetadataError>;
    async fn put_lifecycle_rule(&self, rule: &LifecycleRule) -> Result<(), MetadataError>;
    async fn list_lifecycle_rules(
        &self,
        bucket: Option<BucketId>,
    ) -> Result<Vec<LifecycleRule>, MetadataError>;
    async fn delete_lifecycle_rule(&self, id: LifecycleRuleId) -> Result<(), MetadataError>;
    async fn storage_usage(&self) -> Result<StorageUsage, MetadataError>;
    /// Returns per-bucket accounting for every bucket in one pass.
    ///
    /// Callers that render a bucket table need this to avoid issuing one request
    /// per bucket.
    async fn bucket_usage(
        &self,
    ) -> Result<std::collections::BTreeMap<BucketId, BucketUsageSummary>, MetadataError>;
    async fn pending_cleanup(&self, limit: usize) -> Result<Vec<ObjectId>, MetadataError>;
    async fn complete_cleanup(&self, id: ObjectId) -> Result<(), MetadataError>;
    /// Returns whether any durable object version or multipart part owns a payload.
    async fn payload_referenced(&self, id: ObjectId) -> Result<bool, MetadataError>;
    async fn list_payload_references(
        &self,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<PayloadReferencePage, MetadataError>;
    async fn check_ready(&self) -> Result<(), MetadataError>;
}
