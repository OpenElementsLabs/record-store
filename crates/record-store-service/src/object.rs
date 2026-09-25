//! Shared bucket and object application services.

use std::sync::{Arc, atomic::Ordering};

use futures_util::StreamExt;
use record_store_core::{
    BucketName, ObjectKey, ObjectLockState, ObjectMetadata, ObjectVersionRecord, VersionId,
    WriteOrigin,
};
use record_store_metadata::{
    ListObjectVersionsRequest as MetadataVersionListRequest, MetadataRepository,
};
use record_store_storage::{
    DeleteObjectRequest, DeleteObjectVersionRequest, GetObjectRequest, GetObjectVersionRequest,
    HeadObjectRequest, ObjectStore, PutObjectRequest, PutObjectResult, StorageError,
};

use crate::error::map_storage;
use crate::lock::LockPolicy;
use crate::services::BucketCoordinator;
use crate::*;

/// Object lifecycle service shared by every object protocol.
pub struct ObjectService {
    pub(crate) storage: Arc<dyn ObjectStore>,
    pub(crate) metadata: Arc<dyn MetadataRepository>,
    pub(crate) coordinator: Arc<BucketCoordinator>,
    pub(crate) admission: Arc<crate::admission::Admission>,
    pub(crate) metrics: Arc<ServiceMetrics>,
    pub(crate) maximum_custom_metadata_entries: usize,
    pub(crate) maximum_custom_metadata_bytes: usize,
    pub(crate) policy: Arc<LockPolicy>,
}

impl ObjectService {
    /// Streams and commits an object after validating protocol-independent limits.
    pub async fn put(&self, request: ServicePutRequest) -> Result<PutObjectResult, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        self.validate_metadata(&request)?;
        let permit = self.acquire().await?;
        let bucket = self.resolve_bucket(&request.bucket).await?;
        let lock = self.coordinator.lock(bucket.id)?;
        let _bucket_guard = lock.read().await;
        let object_lock = ObjectLockService::initial_state(&bucket, request.object_lock)?;
        let result = self
            .storage
            .put(PutObjectRequest {
                bucket_id: bucket.id,
                key: request.key,
                content_type: request.content_type,
                custom_metadata: request.custom_metadata,
                expected_checksum: request.expected_checksum,
                object_id: None,
                protocol_etag: None,
                object_lock,
                origin: WriteOrigin::Direct,
                body: request.body,
            })
            .await
            .map_err(map_storage);
        drop(permit);
        match result {
            Ok(result) => {
                self.metrics
                    .upload_bytes
                    .fetch_add(result.metadata.size, Ordering::Relaxed);
                Ok(result)
            }
            Err(error) => {
                self.metrics.errors.fetch_add(1, Ordering::Relaxed);
                Err(error)
            }
        }
    }

    /// Opens a streaming object or range read.
    pub async fn get(
        &self,
        bucket_name: &BucketName,
        key: ObjectKey,
        range: Option<record_store_core::ByteRange>,
    ) -> Result<ServiceGetResult, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let permit = self.acquire().await?;
        let bucket = self.resolve_bucket(bucket_name).await?;
        let lock = self.coordinator.lock(bucket.id)?;
        let bucket_guard = lock.read_owned().await;
        let result = self
            .storage
            .get(GetObjectRequest {
                bucket_id: bucket.id,
                key,
                range,
            })
            .await
            .map_err(map_storage)?;
        let metrics = Arc::clone(&self.metrics);
        let body = result.body.map(move |item| {
            let _keep_alive = (&permit, &bucket_guard);
            if let Ok(chunk) = &item {
                metrics
                    .download_bytes
                    .fetch_add(chunk.len() as u64, Ordering::Relaxed);
            } else {
                metrics.errors.fetch_add(1, Ordering::Relaxed);
            }
            item
        });
        Ok(ServiceGetResult {
            metadata: result.metadata,
            range: result.range,
            body: Box::pin(body),
        })
    }

    /// Opens a streaming immutable historical version or range.
    pub async fn get_version(
        &self,
        bucket_name: &BucketName,
        key: ObjectKey,
        version_id: VersionId,
        range: Option<record_store_core::ByteRange>,
    ) -> Result<ServiceGetResult, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let permit = self.acquire().await?;
        let bucket = self.resolve_bucket(bucket_name).await?;
        let lock = self.coordinator.lock(bucket.id)?;
        let bucket_guard = lock.read_owned().await;
        let result = self
            .storage
            .get_version(GetObjectVersionRequest {
                bucket_id: bucket.id,
                key,
                version_id,
                range,
            })
            .await
            .map_err(map_storage)?;
        let metrics = Arc::clone(&self.metrics);
        let body = result.body.map(move |item| {
            let _keep_alive = (&permit, &bucket_guard);
            if let Ok(chunk) = &item {
                metrics
                    .download_bytes
                    .fetch_add(chunk.len() as u64, Ordering::Relaxed);
            } else {
                metrics.errors.fetch_add(1, Ordering::Relaxed);
            }
            item
        });
        Ok(ServiceGetResult {
            metadata: result.metadata,
            range: result.range,
            body: Box::pin(body),
        })
    }

    /// Opens the special S3 null version retained by disabled/suspended writes.
    pub async fn get_null_version(
        &self,
        bucket_name: &BucketName,
        key: ObjectKey,
        range: Option<record_store_core::ByteRange>,
    ) -> Result<ServiceGetResult, ServiceError> {
        let bucket = self.resolve_bucket(bucket_name).await?;
        let record = self
            .metadata
            .get_null_version(bucket.id, &key)
            .await?
            .ok_or(ServiceError::ObjectNotFound)?;
        match record {
            ObjectVersionRecord::Object { metadata, .. } => {
                self.get_version(bucket_name, key, metadata.version_id, range)
                    .await
            }
            ObjectVersionRecord::DeleteMarker { marker, .. } => {
                Err(ServiceError::DeleteMarker(marker.version_id))
            }
        }
    }

    /// Returns persisted metadata without reading payload bytes.
    pub async fn head(
        &self,
        bucket_name: &BucketName,
        key: ObjectKey,
    ) -> Result<ObjectMetadata, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        let bucket = self.resolve_bucket(bucket_name).await?;
        let lock = self.coordinator.lock(bucket.id)?;
        let _guard = lock.read().await;
        self.storage
            .head(HeadObjectRequest {
                bucket_id: bucket.id,
                key,
            })
            .await
            .map_err(map_storage)
    }

    /// Returns immutable historical object metadata without reading bytes.
    pub async fn head_version(
        &self,
        bucket_name: &BucketName,
        key: ObjectKey,
        version_id: VersionId,
    ) -> Result<ObjectMetadata, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        let bucket = self.resolve_bucket(bucket_name).await?;
        match self
            .metadata
            .get_object_version(bucket.id, &key, version_id)
            .await?
            .ok_or(ServiceError::ObjectNotFound)?
        {
            ObjectVersionRecord::Object { metadata, .. } => Ok(metadata),
            ObjectVersionRecord::DeleteMarker { marker, .. } => {
                Err(ServiceError::DeleteMarker(marker.version_id))
            }
        }
    }

    /// Returns metadata for the special S3 null version.
    pub async fn head_null_version(
        &self,
        bucket_name: &BucketName,
        key: ObjectKey,
    ) -> Result<ObjectMetadata, ServiceError> {
        let bucket = self.resolve_bucket(bucket_name).await?;
        match self
            .metadata
            .get_null_version(bucket.id, &key)
            .await?
            .ok_or(ServiceError::ObjectNotFound)?
        {
            ObjectVersionRecord::Object { metadata, .. } => Ok(metadata),
            ObjectVersionRecord::DeleteMarker { marker, .. } => {
                Err(ServiceError::DeleteMarker(marker.version_id))
            }
        }
    }

    /// Deletes an object. Returns false when it was already absent.
    pub async fn delete(
        &self,
        bucket_name: &BucketName,
        key: ObjectKey,
    ) -> Result<bool, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        let bucket = self.resolve_bucket(bucket_name).await?;
        let lock = self.coordinator.lock(bucket.id)?;
        let _guard = lock.read().await;
        let result = match self
            .storage
            .delete(DeleteObjectRequest {
                bucket_id: bucket.id,
                key: key.clone(),
            })
            .await
        {
            Ok(result) => result.previously_visible,
            Err(StorageError::ObjectNotFound) => false,
            Err(error) => return Err(map_storage(error)),
        };
        Ok(result)
    }

    /// Deletes an object and returns version/delete-marker protocol details.
    pub async fn delete_detailed(
        &self,
        bucket_name: &BucketName,
        key: ObjectKey,
    ) -> Result<ServiceDeleteResult, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        let bucket = self.resolve_bucket(bucket_name).await?;
        let lock = self.coordinator.lock(bucket.id)?;
        let _guard = lock.read().await;
        let result = match self
            .storage
            .delete(DeleteObjectRequest {
                bucket_id: bucket.id,
                key: key.clone(),
            })
            .await
        {
            Ok(result) => result,
            Err(StorageError::ObjectNotFound) => {
                return Ok(ServiceDeleteResult {
                    delete_marker: None,
                    previously_visible: false,
                });
            }
            Err(error) => return Err(map_storage(error)),
        };
        Ok(ServiceDeleteResult {
            delete_marker: result.delete_marker,
            previously_visible: result.previously_visible,
        })
    }

    /// Permanently removes an explicitly selected immutable version.
    ///
    /// Object Lock is enforced inside the metadata transaction that removes the
    /// version, so a retention placed concurrently cannot be raced. This layer
    /// supplies the clock and bypass inputs it is judged against, and records
    /// an exercised bypass in the durable audit trail.
    pub async fn delete_version(
        &self,
        bucket_name: &BucketName,
        key: ObjectKey,
        version_id: VersionId,
        context: &LockContext,
    ) -> Result<(), ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        let bucket = self.resolve_bucket(bucket_name).await?;
        let lock = self.coordinator.lock(bucket.id)?;
        let _guard = lock.read().await;
        // The bypass is announced before the version can be gone, and the
        // deletion is refused when the announcement cannot be made durable.
        let intent = self
            .policy
            .begin_bypass(
                context,
                "object-lock.bypass-delete-version",
                bucket_name,
                &key,
                version_id,
            )
            .await?;
        let result = self
            .storage
            .delete_version(DeleteObjectVersionRequest {
                bucket_id: bucket.id,
                key: key.clone(),
                version_id,
                release: self.policy.release(context),
            })
            .await
            .map_err(map_storage);
        self.policy
            .complete_bypass(
                intent,
                if result.is_ok() {
                    record_store_audit::AuditResult::Success
                } else {
                    record_store_audit::AuditResult::Denied
                },
            )
            .await;
        result?;
        Ok(())
    }

    /// Permanently removes the special null version.
    pub async fn delete_null_version(
        &self,
        bucket_name: &BucketName,
        key: ObjectKey,
        context: &LockContext,
    ) -> Result<(), ServiceError> {
        let bucket = self.resolve_bucket(bucket_name).await?;
        let record = self
            .metadata
            .get_null_version(bucket.id, &key)
            .await?
            .ok_or(ServiceError::ObjectNotFound)?;
        self.delete_version(bucket_name, key, record.version_id(), context)
            .await
    }

    /// Returns the Object Lock state of one version, for response headers.
    pub async fn version_lock(
        &self,
        version_id: VersionId,
    ) -> Result<ObjectLockState, ServiceError> {
        self.metadata
            .get_object_lock(version_id)
            .await
            .map_err(crate::error::map_metadata)
    }

    /// Lists immutable versions and delete markers without unbounded loading.
    pub async fn list_versions(
        &self,
        request: ServiceListVersionsRequest,
    ) -> Result<ServiceListVersionsResult, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        if request.maximum_keys > 1_000 {
            return Err(ServiceError::InvalidRequest(
                "maximum_keys must not exceed 1000".into(),
            ));
        }
        let bucket = self.resolve_bucket(&request.bucket).await?;
        let page = self
            .metadata
            .list_object_versions(MetadataVersionListRequest {
                bucket_id: bucket.id,
                prefix: request.prefix,
                key_marker: request.key_marker,
                version_id_marker: request.version_id_marker,
                limit: request.maximum_keys,
            })
            .await?;
        Ok(ServiceListVersionsResult {
            versions: page.versions,
            next_key_marker: page.next_key_marker,
            next_version_id_marker: page.next_version_id_marker,
        })
    }
}
