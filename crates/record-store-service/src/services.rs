//! Shared bucket and object application services.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};

use record_store_core::{BucketId, OrganizationId};
use record_store_events::EventRepository;
use record_store_metadata::MetadataRepository;
use record_store_storage::ObjectStore;
use tokio::sync::{RwLock, Semaphore};

use crate::*;

#[derive(Default)]
pub(crate) struct BucketCoordinator {
    pub(crate) locks: Mutex<HashMap<BucketId, Weak<RwLock<()>>>>,
}

impl BucketCoordinator {
    pub(crate) fn lock(&self, bucket_id: BucketId) -> Result<Arc<RwLock<()>>, ServiceError> {
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
