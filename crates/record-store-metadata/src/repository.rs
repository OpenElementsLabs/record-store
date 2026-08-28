//! Durable single-node metadata catalog.

use async_trait::async_trait;
use record_store_core::{
    Bucket, BucketId, BucketName, BucketQuota, CorsConfiguration, LifecycleRule, LifecycleRuleId,
    MultipartUpload, ObjectId, ObjectKey, ObjectMetadata, ObjectVersionRecord, PartNumber,
    StorageUsage, UploadId, UploadedPart, VersionId, VersioningState,
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
    async fn delete_bucket(&self, name: &BucketName) -> Result<Bucket, MetadataError>;
    async fn put_object(
        &self,
        metadata: &ObjectMetadata,
    ) -> Result<ObjectCommitResult, MetadataError>;
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
    async fn delete_object_version(
        &self,
        bucket: BucketId,
        key: &ObjectKey,
        version: VersionId,
    ) -> Result<Option<DeleteVersionResult>, MetadataError>;
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
