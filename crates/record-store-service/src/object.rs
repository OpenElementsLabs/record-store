//! Shared bucket and object application services.

use std::sync::{Arc, atomic::Ordering};

use futures_util::StreamExt;
use record_store_core::{BucketName, ObjectKey, ObjectMetadata, ObjectVersionRecord, VersionId};
use record_store_events::{EventRepository, StorageEvent, StorageEventType};
use record_store_metadata::{
    ListObjectVersionsRequest as MetadataVersionListRequest, MetadataRepository,
};
use record_store_storage::{
    DeleteObjectRequest, DeleteObjectVersionRequest, GetObjectRequest, GetObjectVersionRequest,
    HeadObjectRequest, ObjectStore, PutObjectRequest, PutObjectResult, StorageError,
};
use tokio::sync::Semaphore;

use crate::error::map_storage;
use crate::events::publish_event;
use crate::services::BucketCoordinator;
use crate::*;

/// Object lifecycle service shared by every object protocol.
pub struct ObjectService {
    pub(crate) storage: Arc<dyn ObjectStore>,
    pub(crate) metadata: Arc<dyn MetadataRepository>,
    pub(crate) coordinator: Arc<BucketCoordinator>,
    pub(crate) operations: Arc<Semaphore>,
    pub(crate) metrics: Arc<ServiceMetrics>,
    pub(crate) maximum_custom_metadata_entries: usize,
    pub(crate) maximum_custom_metadata_bytes: usize,
    pub(crate) events: Option<Arc<dyn EventRepository>>,
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
        let event_type = if self
            .metadata
            .get_object(bucket.id, &request.key)
            .await?
            .is_some()
        {
            StorageEventType::ObjectUpdated
        } else {
            StorageEventType::ObjectCreated
        };
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
                publish_event(
                    &self.events,
                    StorageEvent::new(event_type, bucket.name.as_str()).object(
                        result.metadata.key.as_str(),
                        Some(result.metadata.version_id),
                        Some(result.metadata.size),
                    ),
                )
                .await;
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
        if result {
            publish_event(
                &self.events,
                StorageEvent::new(StorageEventType::ObjectDeleted, bucket.name.as_str()).object(
                    key.as_str(),
                    None,
                    None,
                ),
            )
            .await;
        }
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
        if result.previously_visible || result.delete_marker.is_some() {
            publish_event(
                &self.events,
                StorageEvent::new(StorageEventType::ObjectDeleted, bucket.name.as_str()).object(
                    key.as_str(),
                    result
                        .delete_marker
                        .as_ref()
                        .map(|marker| marker.version_id),
                    None,
                ),
            )
            .await;
        }
        Ok(ServiceDeleteResult {
            delete_marker: result.delete_marker,
            previously_visible: result.previously_visible,
        })
    }

    /// Permanently removes an explicitly selected immutable version.
    pub async fn delete_version(
        &self,
        bucket_name: &BucketName,
        key: ObjectKey,
        version_id: VersionId,
    ) -> Result<(), ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        let bucket = self.resolve_bucket(bucket_name).await?;
        let lock = self.coordinator.lock(bucket.id)?;
        let _guard = lock.read().await;
        self.storage
            .delete_version(DeleteObjectVersionRequest {
                bucket_id: bucket.id,
                key: key.clone(),
                version_id,
            })
            .await
            .map_err(map_storage)?;
        publish_event(
            &self.events,
            StorageEvent::new(StorageEventType::ObjectDeleted, bucket.name.as_str()).object(
                key.as_str(),
                Some(version_id),
                None,
            ),
        )
        .await;
        Ok(())
    }

    /// Permanently removes the special null version.
    pub async fn delete_null_version(
        &self,
        bucket_name: &BucketName,
        key: ObjectKey,
    ) -> Result<(), ServiceError> {
        let bucket = self.resolve_bucket(bucket_name).await?;
        let record = self
            .metadata
            .get_null_version(bucket.id, &key)
            .await?
            .ok_or(ServiceError::ObjectNotFound)?;
        self.delete_version(bucket_name, key, record.version_id())
            .await
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
