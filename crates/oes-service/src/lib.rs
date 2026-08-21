//! Shared bucket and object application services.

use std::{
    collections::{BTreeSet, HashMap},
    io,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

use chrono::Utc;
use futures_util::{StreamExt, TryStreamExt};
use oes_core::{
    Bucket, BucketId, BucketName, BucketQuota, Checksum, CompletedPart, CoreError, MultipartUpload,
    MultipartUploadState, ObjectKey, ObjectMetadata, ObjectVersionRecord, OrganizationId,
    PartNumber, StorageUsage, UploadId, UploadedPart, VersionId, VersioningState,
};
use oes_events::{EventRepository, StorageEvent, StorageEventType};
use oes_metadata::{
    ListMultipartUploadsRequest as MetadataMultipartListRequest,
    ListObjectVersionsRequest as MetadataVersionListRequest,
    ListObjectsRequest as MetadataListRequest, ListedObjectVersion, MetadataError,
    MetadataRepository,
};
use oes_storage::{
    CompleteMultipartRequest, DeleteObjectRequest, DeleteObjectVersionRequest, DownloadStream,
    GetObjectRequest, GetObjectVersionRequest, HeadObjectRequest, ObjectStore,
    PutMultipartPartRequest, PutObjectRequest, PutObjectResult, StorageError, StorageInspection,
    StorageRepairRequest, StorageRepairResult, StorageStatus, UploadStream, VerifyObjectRequest,
    upload_stream,
};
use thiserror::Error;
use tokio::sync::{RwLock, Semaphore};

/// Shared service-layer operation metrics without high-cardinality labels.
#[derive(Debug, Default)]
pub struct ServiceMetrics {
    requests: AtomicU64,
    errors: AtomicU64,
    upload_bytes: AtomicU64,
    download_bytes: AtomicU64,
}

impl ServiceMetrics {
    /// Returns a point-in-time metric snapshot.
    #[must_use]
    pub fn snapshot(&self) -> ServiceMetricsSnapshot {
        ServiceMetricsSnapshot {
            requests: self.requests.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            upload_bytes: self.upload_bytes.load(Ordering::Relaxed),
            download_bytes: self.download_bytes.load(Ordering::Relaxed),
        }
    }
}

/// Copyable metrics snapshot for native status and Prometheus exposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceMetricsSnapshot {
    /// Total service operations started.
    pub requests: u64,
    /// Total service operations that returned an error.
    pub errors: u64,
    /// Bytes successfully committed through PUT operations.
    pub upload_bytes: u64,
    /// Bytes yielded through download streams.
    pub download_bytes: u64,
}

#[derive(Default)]
struct BucketCoordinator {
    locks: Mutex<HashMap<BucketId, Weak<RwLock<()>>>>,
}

impl BucketCoordinator {
    fn lock(&self, bucket_id: BucketId) -> Result<Arc<RwLock<()>>, ServiceError> {
        let mut locks = self.locks.lock().map_err(|_| ServiceError::Coordination)?;
        if let Some(lock) = locks.get(&bucket_id).and_then(Weak::upgrade) {
            return Ok(lock);
        }
        locks.retain(|_, lock| lock.strong_count() > 0);
        let lock = Arc::new(RwLock::new(()));
        locks.insert(bucket_id, Arc::downgrade(&lock));
        Ok(lock)
    }
}

/// Shared application services used by S3 and native interfaces.
#[derive(Clone)]
pub struct Services {
    /// Bucket lifecycle service.
    pub buckets: Arc<BucketService>,
    /// Object lifecycle service.
    pub objects: Arc<ObjectService>,
    /// Low-cardinality service metrics.
    pub metrics: Arc<ServiceMetrics>,
}

impl Services {
    /// Constructs services with shared per-bucket coordination and backpressure.
    #[must_use]
    pub fn new(
        storage: Arc<dyn ObjectStore>,
        metadata: Arc<dyn MetadataRepository>,
        owner: OrganizationId,
        limits: ServiceLimits,
    ) -> Self {
        Self::new_with_events(storage, metadata, owner, limits, None)
    }

    /// Constructs services with an optional durable storage-event outbox.
    #[must_use]
    pub fn new_with_events(
        storage: Arc<dyn ObjectStore>,
        metadata: Arc<dyn MetadataRepository>,
        owner: OrganizationId,
        limits: ServiceLimits,
        events: Option<Arc<dyn EventRepository>>,
    ) -> Self {
        let coordinator = Arc::new(BucketCoordinator::default());
        let operations = Arc::new(Semaphore::new(limits.maximum_concurrent_operations));
        let metrics = Arc::new(ServiceMetrics::default());
        Self {
            buckets: Arc::new(BucketService {
                metadata: Arc::clone(&metadata),
                coordinator: Arc::clone(&coordinator),
                operations: Arc::clone(&operations),
                metrics: Arc::clone(&metrics),
                owner,
                events: events.clone(),
            }),
            objects: Arc::new(ObjectService {
                storage,
                metadata,
                coordinator,
                operations,
                metrics: Arc::clone(&metrics),
                maximum_custom_metadata_entries: limits.maximum_custom_metadata_entries,
                maximum_custom_metadata_bytes: limits.maximum_custom_metadata_bytes,
                events,
            }),
            metrics,
        }
    }
}

/// Resource limits enforced consistently across protocol adapters.
#[derive(Debug, Clone, Copy)]
pub struct ServiceLimits {
    /// Maximum concurrent service operations.
    pub maximum_concurrent_operations: usize,
    /// Maximum custom metadata entry count.
    pub maximum_custom_metadata_entries: usize,
    /// Maximum aggregate custom metadata bytes.
    pub maximum_custom_metadata_bytes: usize,
}

/// Bucket lifecycle service.
pub struct BucketService {
    metadata: Arc<dyn MetadataRepository>,
    coordinator: Arc<BucketCoordinator>,
    operations: Arc<Semaphore>,
    metrics: Arc<ServiceMetrics>,
    owner: OrganizationId,
    events: Option<Arc<dyn EventRepository>>,
}

impl BucketService {
    /// Creates a globally unique bucket.
    pub async fn create(&self, name: BucketName) -> Result<Bucket, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        let bucket = Bucket {
            id: BucketId::new(),
            organization_id: self.owner,
            name,
            created_at: Utc::now(),
            versioning: VersioningState::Disabled,
            quota: BucketQuota::default(),
        };
        self.metadata
            .create_bucket(&bucket)
            .await
            .map_err(map_metadata)?;
        publish_event(
            &self.events,
            StorageEvent::new(StorageEventType::BucketCreated, bucket.name.as_str()),
        )
        .await;
        Ok(bucket)
    }

    /// Returns a bucket by name.
    pub async fn head(&self, name: &BucketName) -> Result<Bucket, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        self.resolve(name).await
    }

    /// Lists all buckets in deterministic ascending-name order.
    pub async fn list(&self) -> Result<Vec<Bucket>, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        self.metadata.list_buckets().await.map_err(map_metadata)
    }

    /// Updates explicit bucket versioning state.
    pub async fn set_versioning(
        &self,
        name: &BucketName,
        state: VersioningState,
    ) -> Result<Bucket, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        let bucket = self.resolve(name).await?;
        let lock = self.coordinator.lock(bucket.id)?;
        let _guard = lock.write().await;
        self.metadata
            .set_bucket_versioning(bucket.id, state)
            .await
            .map_err(map_metadata)
    }

    /// Applies transactionally enforced bucket quotas.
    pub async fn set_quota(
        &self,
        name: &BucketName,
        quota: BucketQuota,
    ) -> Result<Bucket, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        let bucket = self.resolve(name).await?;
        let lock = self.coordinator.lock(bucket.id)?;
        let _guard = lock.write().await;
        self.metadata
            .set_bucket_quota(bucket.id, quota)
            .await
            .map_err(map_metadata)
    }

    /// Deletes a bucket only when it is empty and has no active object operation.
    pub async fn delete(&self, name: &BucketName) -> Result<(), ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        let bucket = self.resolve(name).await?;
        let lock = self.coordinator.lock(bucket.id)?;
        let _guard = lock.write().await;
        self.metadata
            .delete_bucket(name)
            .await
            .map_err(map_metadata)?;
        publish_event(
            &self.events,
            StorageEvent::new(StorageEventType::BucketDeleted, name.as_str()),
        )
        .await;
        Ok(())
    }

    async fn resolve(&self, name: &BucketName) -> Result<Bucket, ServiceError> {
        self.metadata
            .get_bucket_by_name(name)
            .await
            .map_err(ServiceError::Metadata)?
            .ok_or(ServiceError::BucketNotFound)
    }

    async fn acquire(&self) -> Result<tokio::sync::OwnedSemaphorePermit, ServiceError> {
        Arc::clone(&self.operations)
            .acquire_owned()
            .await
            .map_err(|_| ServiceError::Unavailable)
    }
}

/// Object lifecycle service shared by every object protocol.
pub struct ObjectService {
    storage: Arc<dyn ObjectStore>,
    metadata: Arc<dyn MetadataRepository>,
    coordinator: Arc<BucketCoordinator>,
    operations: Arc<Semaphore>,
    metrics: Arc<ServiceMetrics>,
    maximum_custom_metadata_entries: usize,
    maximum_custom_metadata_bytes: usize,
    events: Option<Arc<dyn EventRepository>>,
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
        range: Option<oes_core::ByteRange>,
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
        range: Option<oes_core::ByteRange>,
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
        range: Option<oes_core::ByteRange>,
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

    /// Starts a durable resumable multipart upload.
    pub async fn create_multipart(
        &self,
        request: ServiceCreateMultipartRequest,
    ) -> Result<MultipartUpload, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        self.validate_custom_metadata(&request.custom_metadata)?;
        let _permit = self.acquire().await?;
        let bucket = self.resolve_bucket(&request.bucket).await?;
        let upload = MultipartUpload {
            id: UploadId::new(),
            bucket_id: bucket.id,
            key: request.key,
            content_type: request.content_type,
            custom_metadata: request.custom_metadata,
            initiated_at: Utc::now(),
            state: MultipartUploadState::Active,
        };
        self.metadata.create_multipart_upload(&upload).await?;
        Ok(upload)
    }

    /// Streams one multipart part directly to durable immutable storage.
    pub async fn upload_part(
        &self,
        request: ServiceUploadPartRequest,
    ) -> Result<UploadedPart, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        let bucket = self.resolve_bucket(&request.bucket).await?;
        let upload = self
            .metadata
            .get_multipart_upload(request.upload_id)
            .await?
            .ok_or(ServiceError::MultipartUploadNotFound)?;
        if upload.bucket_id != bucket.id || upload.key != request.key {
            return Err(ServiceError::MultipartUploadNotFound);
        }
        self.storage
            .put_multipart_part(PutMultipartPartRequest {
                upload_id: upload.id,
                number: request.number,
                expected_checksum: request.expected_checksum,
                body: request.body,
            })
            .await
            .map_err(map_storage)
    }

    /// Returns a bounded ascending part page.
    pub async fn list_parts(
        &self,
        bucket_name: &BucketName,
        key: &ObjectKey,
        upload_id: UploadId,
        after: Option<PartNumber>,
        maximum_parts: usize,
    ) -> Result<Vec<UploadedPart>, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        if maximum_parts > 1_001 {
            return Err(ServiceError::InvalidRequest(
                "maximum_parts must not exceed 1001".into(),
            ));
        }
        let bucket = self.resolve_bucket(bucket_name).await?;
        let upload = self
            .metadata
            .get_multipart_upload(upload_id)
            .await?
            .ok_or(ServiceError::MultipartUploadNotFound)?;
        if upload.bucket_id != bucket.id || upload.key != *key {
            return Err(ServiceError::MultipartUploadNotFound);
        }
        self.metadata
            .list_multipart_parts(upload_id, after, maximum_parts)
            .await
            .map_err(ServiceError::Metadata)
    }

    /// Validates a manifest and atomically publishes the composed object.
    pub async fn complete_multipart(
        &self,
        request: ServiceCompleteMultipartRequest,
    ) -> Result<PutObjectResult, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        let bucket = self.resolve_bucket(&request.bucket).await?;
        let lock = self.coordinator.lock(bucket.id)?;
        let _guard = lock.read().await;
        let upload = self
            .metadata
            .get_multipart_upload(request.upload_id)
            .await?
            .ok_or(ServiceError::MultipartUploadNotFound)?;
        if upload.bucket_id != bucket.id || upload.key != request.key {
            return Err(ServiceError::MultipartUploadNotFound);
        }
        if request.manifest.is_empty() || request.manifest.len() > PartNumber::MAX as usize {
            return Err(ServiceError::InvalidPart);
        }
        if !request
            .manifest
            .windows(2)
            .all(|parts| parts[0].number < parts[1].number)
        {
            return Err(ServiceError::InvalidPartOrder);
        }
        let persisted = self
            .metadata
            .list_multipart_parts(upload.id, None, PartNumber::MAX as usize)
            .await?;
        let by_number = persisted
            .into_iter()
            .map(|part| (part.number, part))
            .collect::<std::collections::BTreeMap<_, _>>();
        let mut parts = Vec::with_capacity(request.manifest.len());
        for (index, item) in request.manifest.iter().enumerate() {
            let part = by_number
                .get(&item.number)
                .filter(|part| part.etag == item.etag)
                .ok_or(ServiceError::InvalidPart)?;
            if index + 1 != request.manifest.len() && part.size < 5 * 1024 * 1024 {
                return Err(ServiceError::EntityTooSmall);
            }
            parts.push(part.clone());
        }
        let result = self
            .storage
            .complete_multipart(CompleteMultipartRequest { upload, parts })
            .await
            .map_err(map_storage)?;
        self.metrics
            .upload_bytes
            .fetch_add(result.metadata.size, Ordering::Relaxed);
        publish_event(
            &self.events,
            StorageEvent::new(StorageEventType::MultipartCompleted, bucket.name.as_str()).object(
                result.metadata.key.as_str(),
                Some(result.metadata.version_id),
                Some(result.metadata.size),
            ),
        )
        .await;
        Ok(result)
    }

    /// Aborts an upload and schedules all durable parts for cleanup.
    pub async fn abort_multipart(
        &self,
        bucket_name: &BucketName,
        key: &ObjectKey,
        upload_id: UploadId,
    ) -> Result<(), ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        let bucket = self.resolve_bucket(bucket_name).await?;
        let upload = self
            .metadata
            .get_multipart_upload(upload_id)
            .await?
            .ok_or(ServiceError::MultipartUploadNotFound)?;
        if upload.bucket_id != bucket.id || upload.key != *key {
            return Err(ServiceError::MultipartUploadNotFound);
        }
        self.metadata.abort_multipart_upload(upload_id).await?;
        if let Err(error) = self.storage.cleanup_pending(10_000).await {
            tracing::warn!(%error, %upload_id, "multipart abort payload cleanup deferred");
        }
        publish_event(
            &self.events,
            StorageEvent::new(StorageEventType::MultipartAborted, bucket.name.as_str()).object(
                key.as_str(),
                None,
                None,
            ),
        )
        .await;
        Ok(())
    }

    /// Lists active multipart uploads using indexed metadata pagination.
    pub async fn list_multipart_uploads(
        &self,
        request: ServiceListMultipartUploadsRequest,
    ) -> Result<ServiceListMultipartUploadsResult, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        if request.maximum_uploads > 1_000 {
            return Err(ServiceError::InvalidRequest(
                "maximum_uploads must not exceed 1000".into(),
            ));
        }
        let bucket = self.resolve_bucket(&request.bucket).await?;
        let page = self
            .metadata
            .list_multipart_uploads(MetadataMultipartListRequest {
                bucket_id: bucket.id,
                prefix: request.prefix,
                upload_id_marker: request.upload_id_marker,
                limit: request.maximum_uploads,
            })
            .await?;
        Ok(ServiceListMultipartUploadsResult {
            uploads: page.uploads,
            next_upload_id_marker: page.next_upload_id_marker,
        })
    }

    /// Streams a server-side copy without buffering payload bytes.
    pub async fn copy(&self, request: ServiceCopyRequest) -> Result<PutObjectResult, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        self.validate_custom_metadata(&request.replacement_metadata)?;
        let _permit = self.acquire().await?;
        let source_bucket = self.resolve_bucket(&request.source_bucket).await?;
        let destination_bucket = self.resolve_bucket(&request.destination_bucket).await?;
        let event_type = if self
            .metadata
            .get_object(destination_bucket.id, &request.destination_key)
            .await?
            .is_some()
        {
            StorageEventType::ObjectUpdated
        } else {
            StorageEventType::ObjectCreated
        };
        let source = if let Some(version_id) = request.source_version_id {
            self.storage
                .get_version(GetObjectVersionRequest {
                    bucket_id: source_bucket.id,
                    key: request.source_key,
                    version_id,
                    range: None,
                })
                .await
        } else {
            self.storage
                .get(GetObjectRequest {
                    bucket_id: source_bucket.id,
                    key: request.source_key,
                    range: None,
                })
                .await
        }
        .map_err(map_storage)?;
        let (content_type, custom_metadata) = match request.metadata_directive {
            CopyMetadataDirective::Copy => (
                source.metadata.content_type.clone(),
                source.metadata.custom_metadata.clone(),
            ),
            CopyMetadataDirective::Replace => (request.content_type, request.replacement_metadata),
        };
        let body = source
            .body
            .map_err(|error| io::Error::other(error.to_string()));
        let result = self
            .storage
            .put(PutObjectRequest {
                bucket_id: destination_bucket.id,
                key: request.destination_key.clone(),
                content_type,
                custom_metadata,
                expected_checksum: Some(source.metadata.checksum),
                object_id: None,
                protocol_etag: None,
                body: upload_stream(body),
            })
            .await
            .map_err(map_storage)?;
        self.metrics
            .upload_bytes
            .fetch_add(result.metadata.size, Ordering::Relaxed);
        publish_event(
            &self.events,
            StorageEvent::new(event_type, destination_bucket.name.as_str()).object(
                request.destination_key.as_str(),
                Some(result.metadata.version_id),
                Some(result.metadata.size),
            ),
        )
        .await;
        Ok(result)
    }

    /// Restores historical bytes by creating a fresh current version.
    pub async fn restore_version(
        &self,
        bucket_name: &BucketName,
        key: ObjectKey,
        version_id: VersionId,
    ) -> Result<PutObjectResult, ServiceError> {
        let result = self
            .copy(ServiceCopyRequest {
                source_bucket: bucket_name.clone(),
                source_key: key.clone(),
                source_version_id: Some(version_id),
                destination_bucket: bucket_name.clone(),
                destination_key: key,
                metadata_directive: CopyMetadataDirective::Copy,
                content_type: None,
                replacement_metadata: Default::default(),
            })
            .await?;
        publish_event(
            &self.events,
            StorageEvent::new(StorageEventType::ObjectRestored, bucket_name.as_str()).object(
                result.metadata.key.as_str(),
                Some(result.metadata.version_id),
                Some(result.metadata.size),
            ),
        )
        .await;
        Ok(result)
    }

    /// Lists objects without loading the complete bucket into memory.
    pub async fn list(
        &self,
        request: ServiceListRequest,
    ) -> Result<ServiceListResult, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        let bucket = self.resolve_bucket(&request.bucket).await?;
        let lock = self.coordinator.lock(bucket.id)?;
        let _guard = lock.read().await;
        if request.maximum_keys > 1_000 {
            return Err(ServiceError::InvalidRequest(
                "maximum_keys must not exceed 1000".into(),
            ));
        }
        let mut result = ServiceListResult::default();
        if request.maximum_keys == 0 {
            return Ok(result);
        }
        let mut start_after = request.start_after;
        let mut at_capacity = false;
        let mut last_scanned_key = None;
        loop {
            let page = self
                .metadata
                .list_objects(MetadataListRequest {
                    bucket_id: bucket.id,
                    prefix: request.prefix.clone(),
                    start_after: start_after.clone(),
                    limit: 256,
                })
                .await?;
            if page.objects.is_empty() {
                break;
            }
            for metadata in &page.objects {
                let key = metadata.key.as_str();
                let common_prefix = request.delimiter.as_ref().and_then(|delimiter| {
                    key.strip_prefix(&request.prefix).and_then(|suffix| {
                        suffix.find(delimiter).map(|position| {
                            format!("{}{}{}", request.prefix, &suffix[..position], delimiter)
                        })
                    })
                });
                if at_capacity {
                    if common_prefix
                        .as_ref()
                        .is_some_and(|prefix| result.common_prefixes.contains(prefix))
                    {
                        last_scanned_key = Some(key.to_owned());
                        continue;
                    }
                    result.is_truncated = true;
                    result.next_marker = last_scanned_key;
                    return Ok(result);
                }

                if let Some(common_prefix) = common_prefix {
                    result.common_prefixes.insert(common_prefix);
                } else {
                    result.objects.push(metadata.clone());
                }
                last_scanned_key = Some(key.to_owned());
                at_capacity = result.entry_count() >= request.maximum_keys;
            }
            if page.next_key.is_none() {
                break;
            }
            start_after = page.next_key;
        }
        Ok(result)
    }

    /// Explicitly verifies a stored payload checksum.
    pub async fn verify(
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
            .verify(VerifyObjectRequest {
                bucket_id: bucket.id,
                key,
            })
            .await
            .map_err(map_storage)
    }

    /// Returns aggregate logical usage counters.
    pub async fn usage(&self) -> Result<StorageUsage, ServiceError> {
        self.metadata
            .storage_usage()
            .await
            .map_err(ServiceError::Metadata)
    }

    /// Returns filesystem capacity and temporary usage.
    pub async fn status(&self) -> Result<StorageStatus, ServiceError> {
        self.storage.status().await.map_err(ServiceError::Storage)
    }

    /// Performs a bounded, read-only consistency inspection.
    pub async fn inspect(&self, maximum_entries: usize) -> Result<StorageInspection, ServiceError> {
        self.storage
            .inspect(maximum_entries)
            .await
            .map_err(ServiceError::Storage)
    }

    /// Performs an explicitly selected repair; callers should default to dry-run.
    pub async fn repair(
        &self,
        request: StorageRepairRequest,
    ) -> Result<StorageRepairResult, ServiceError> {
        self.storage
            .repair(request)
            .await
            .map_err(ServiceError::Storage)
    }

    fn validate_metadata(&self, request: &ServicePutRequest) -> Result<(), ServiceError> {
        self.validate_custom_metadata(&request.custom_metadata)
    }

    fn validate_custom_metadata(
        &self,
        custom_metadata: &std::collections::BTreeMap<String, String>,
    ) -> Result<(), ServiceError> {
        if custom_metadata.len() > self.maximum_custom_metadata_entries {
            return Err(ServiceError::MetadataTooLarge);
        }
        let bytes = custom_metadata.iter().fold(0_usize, |total, (key, value)| {
            total.saturating_add(key.len()).saturating_add(value.len())
        });
        if bytes > self.maximum_custom_metadata_bytes {
            return Err(ServiceError::MetadataTooLarge);
        }
        Ok(())
    }

    async fn resolve_bucket(&self, name: &BucketName) -> Result<Bucket, ServiceError> {
        self.metadata
            .get_bucket_by_name(name)
            .await?
            .ok_or(ServiceError::BucketNotFound)
    }

    async fn acquire(&self) -> Result<tokio::sync::OwnedSemaphorePermit, ServiceError> {
        Arc::clone(&self.operations)
            .acquire_owned()
            .await
            .map_err(|_| ServiceError::Unavailable)
    }
}

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
    pub expected_checksum: Option<oes_core::Checksum>,
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
    pub range: Option<oes_core::ResolvedByteRange>,
    /// Streaming payload.
    pub body: DownloadStream,
}

/// Detailed ordinary-delete outcome for S3 delete-marker headers.
#[derive(Debug, Clone)]
pub struct ServiceDeleteResult {
    pub delete_marker: Option<oes_core::DeleteMarker>,
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
    fn entry_count(&self) -> usize {
        self.objects.len() + self.common_prefixes.len()
    }
}

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
}

fn map_metadata(error: MetadataError) -> ServiceError {
    match error {
        MetadataError::BucketAlreadyExists => ServiceError::BucketAlreadyExists,
        MetadataError::BucketNotFound => ServiceError::BucketNotFound,
        MetadataError::BucketNotEmpty => ServiceError::BucketNotEmpty,
        MetadataError::MultipartUploadNotFound => ServiceError::MultipartUploadNotFound,
        MetadataError::QuotaExceeded => ServiceError::QuotaExceeded,
        error => ServiceError::Metadata(error),
    }
}

fn map_storage(error: StorageError) -> ServiceError {
    match error {
        StorageError::BucketNotFound => ServiceError::BucketNotFound,
        StorageError::ObjectNotFound => ServiceError::ObjectNotFound,
        StorageError::DeleteMarker { version_id } => ServiceError::DeleteMarker(version_id),
        StorageError::Metadata(MetadataError::MultipartUploadNotFound) => {
            ServiceError::MultipartUploadNotFound
        }
        StorageError::Metadata(MetadataError::QuotaExceeded) => ServiceError::QuotaExceeded,
        error => ServiceError::Storage(error),
    }
}

async fn publish_event(events: &Option<Arc<dyn EventRepository>>, event: StorageEvent) {
    let Some(events) = events else { return };
    if let Err(error) = events.publish(&event).await {
        tracing::error!(event_id = %event.id, %error, "durable storage event publication failed");
    }
}
