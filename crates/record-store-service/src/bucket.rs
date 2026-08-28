//! Shared bucket and object application services.

use std::sync::{Arc, atomic::Ordering};

use chrono::Utc;
use record_store_core::{
    Bucket, BucketId, BucketName, BucketQuota, CorsConfiguration, OrganizationId, VersioningState,
};
use record_store_events::{EventRepository, StorageEvent, StorageEventType};
use record_store_metadata::MetadataRepository;
use tokio::sync::Semaphore;

use crate::error::map_metadata;
use crate::events::publish_event;
use crate::services::BucketCoordinator;
use crate::*;

/// Bucket lifecycle service.
pub struct BucketService {
    pub(crate) metadata: Arc<dyn MetadataRepository>,
    pub(crate) coordinator: Arc<BucketCoordinator>,
    pub(crate) operations: Arc<Semaphore>,
    pub(crate) metrics: Arc<ServiceMetrics>,
    pub(crate) owner: OrganizationId,
    pub(crate) events: Option<Arc<dyn EventRepository>>,
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
            durability_policy: None,
            cors: None,
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

    /// Replaces a bucket's browser CORS configuration after validating every rule.
    pub async fn set_cors(
        &self,
        name: &BucketName,
        configuration: CorsConfiguration,
    ) -> Result<Bucket, ServiceError> {
        configuration.validate()?;
        self.update_cors(name, Some(configuration)).await
    }

    /// Removes a bucket's browser CORS configuration.
    pub async fn delete_cors(&self, name: &BucketName) -> Result<Bucket, ServiceError> {
        self.update_cors(name, None).await
    }

    async fn update_cors(
        &self,
        name: &BucketName,
        configuration: Option<CorsConfiguration>,
    ) -> Result<Bucket, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        let bucket = self.resolve(name).await?;
        let lock = self.coordinator.lock(bucket.id)?;
        let _guard = lock.write().await;
        self.metadata
            .set_bucket_cors(bucket.id, configuration)
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

    pub(crate) async fn acquire(&self) -> Result<tokio::sync::OwnedSemaphorePermit, ServiceError> {
        Arc::clone(&self.operations)
            .acquire_owned()
            .await
            .map_err(|_| ServiceError::Unavailable)
    }
}
