//! Shared bucket and object application services.

use std::{
    collections::{BTreeSet, HashMap},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

use chrono::Utc;
use futures_util::StreamExt;
use oes_core::{
    Bucket, BucketId, BucketName, CoreError, ObjectKey, ObjectMetadata, OrganizationId,
    StorageUsage,
};
use oes_metadata::{ListObjectsRequest as MetadataListRequest, MetadataError, MetadataRepository};
use oes_storage::{
    DeleteObjectRequest, DownloadStream, GetObjectRequest, HeadObjectRequest, ObjectStore,
    PutObjectRequest, PutObjectResult, StorageError, StorageStatus, UploadStream,
    VerifyObjectRequest,
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
            }),
            objects: Arc::new(ObjectService {
                storage,
                metadata,
                coordinator,
                operations,
                metrics: Arc::clone(&metrics),
                maximum_custom_metadata_entries: limits.maximum_custom_metadata_entries,
                maximum_custom_metadata_bytes: limits.maximum_custom_metadata_bytes,
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
        };
        self.metadata
            .create_bucket(&bucket)
            .await
            .map_err(map_metadata)?;
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
        let result = self
            .storage
            .put(PutObjectRequest {
                bucket_id: bucket.id,
                key: request.key,
                content_type: request.content_type,
                custom_metadata: request.custom_metadata,
                expected_checksum: request.expected_checksum,
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
        match self
            .storage
            .delete(DeleteObjectRequest {
                bucket_id: bucket.id,
                key,
            })
            .await
        {
            Ok(()) => Ok(true),
            Err(StorageError::ObjectNotFound) => Ok(false),
            Err(error) => Err(map_storage(error)),
        }
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

    fn validate_metadata(&self, request: &ServicePutRequest) -> Result<(), ServiceError> {
        if request.custom_metadata.len() > self.maximum_custom_metadata_entries {
            return Err(ServiceError::MetadataTooLarge);
        }
        let bytes = request
            .custom_metadata
            .iter()
            .fold(0_usize, |total, (key, value)| {
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

/// Service-layer streaming read result.
pub struct ServiceGetResult {
    /// Persisted metadata.
    pub metadata: ObjectMetadata,
    /// Resolved range.
    pub range: Option<oes_core::ResolvedByteRange>,
    /// Streaming payload.
    pub body: DownloadStream,
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
        error => ServiceError::Metadata(error),
    }
}

fn map_storage(error: StorageError) -> ServiceError {
    match error {
        StorageError::BucketNotFound => ServiceError::BucketNotFound,
        StorageError::ObjectNotFound => ServiceError::ObjectNotFound,
        error => ServiceError::Storage(error),
    }
}
