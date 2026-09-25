//! Shared bucket and object application services.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};

use record_store_audit::AuditRepository;
use record_store_core::{BucketId, OrganizationId};
use record_store_metadata::MetadataRepository;
use record_store_storage::ObjectStore;
use tokio::sync::RwLock;

use crate::lock::LockPolicy;
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
    /// Object Lock retention and legal-hold service.
    pub locks: Arc<ObjectLockService>,
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
        Self::build(storage, metadata, owner, limits, None)
    }

    /// Constructs services with a durable audit trail for Object Lock bypasses.
    ///
    /// A governance bypass is the one way a retained version leaves before its
    /// date, so the deployment that enables Object Lock wants it recorded.
    ///
    /// Storage events are not wired in here. They are journalled by the catalog
    /// inside the transaction that commits each mutation, and moved into the
    /// delivery outbox by [`crate::StorageEventPump`]; a service that published
    /// them itself could only do so after the commit, which is the window that
    /// loses them.
    #[must_use]
    pub fn new_with_audit(
        storage: Arc<dyn ObjectStore>,
        metadata: Arc<dyn MetadataRepository>,
        owner: OrganizationId,
        limits: ServiceLimits,
        audit: Arc<dyn AuditRepository>,
    ) -> Self {
        Self::build(storage, metadata, owner, limits, Some(audit))
    }

    fn build(
        storage: Arc<dyn ObjectStore>,
        metadata: Arc<dyn MetadataRepository>,
        owner: OrganizationId,
        limits: ServiceLimits,
        audit: Option<Arc<dyn AuditRepository>>,
    ) -> Self {
        let coordinator = Arc::new(BucketCoordinator::default());
        let metrics = Arc::new(ServiceMetrics::default());
        let admission = Arc::new(crate::admission::Admission::new(
            limits.maximum_concurrent_operations,
            std::time::Duration::from_secs(u64::from(limits.admission_wait_limit_seconds)),
            Arc::clone(&metrics),
        ));
        let policy = Arc::new(LockPolicy {
            clock_tolerance_seconds: limits.object_lock.clock_backwards_tolerance_seconds,
            audit,
        });
        Self {
            buckets: Arc::new(BucketService {
                metadata: Arc::clone(&metadata),
                coordinator: Arc::clone(&coordinator),
                admission: Arc::clone(&admission),
                metrics: Arc::clone(&metrics),
                owner,
            }),
            objects: Arc::new(ObjectService {
                storage,
                metadata: Arc::clone(&metadata),
                coordinator: Arc::clone(&coordinator),
                admission: Arc::clone(&admission),
                metrics: Arc::clone(&metrics),
                maximum_custom_metadata_entries: limits.maximum_custom_metadata_entries,
                maximum_custom_metadata_bytes: limits.maximum_custom_metadata_bytes,
                policy: Arc::clone(&policy),
            }),
            locks: Arc::new(ObjectLockService {
                metadata,
                coordinator,
                admission,
                metrics: Arc::clone(&metrics),
                policy,
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
    /// How long an operation may wait for a concurrency permit before it is
    /// refused with a retryable error.
    ///
    /// Without a bound here the concurrency limit only bounds the work in
    /// flight; everything beyond it queues for as long as clients are willing
    /// to wait, and that queue is what runs a small deployment out of memory.
    pub admission_wait_limit_seconds: u32,
    /// Maximum custom metadata entry count.
    pub maximum_custom_metadata_entries: usize,
    /// Maximum aggregate custom metadata bytes.
    pub maximum_custom_metadata_bytes: usize,
    /// Object Lock clock settings.
    pub object_lock: ObjectLockLimits,
}

/// How far the wall clock may drift before retention decisions stop.
#[derive(Debug, Clone, Copy)]
pub struct ObjectLockLimits {
    /// How far behind the observed high-water mark the clock may fall before
    /// Object Lock refuses to release anything. This absorbs ordinary NTP
    /// correction; a jump is meant to trip it.
    pub clock_backwards_tolerance_seconds: u32,
}

impl Default for ObjectLockLimits {
    fn default() -> Self {
        Self {
            clock_backwards_tolerance_seconds: 5,
        }
    }
}
