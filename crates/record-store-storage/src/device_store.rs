//! Routing from stable device identities to node-local replica stores.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use async_trait::async_trait;
use record_store_core::{Checksum, DeviceId, ObjectId, PayloadFormat};

use crate::{
    ReadReplicaRequest, ReplicaReadResult, ReplicaStat, ReplicaStore, ReplicaVerification,
    ReplicaWriteResult, StorageError, StorageStatus, WriteReplicaRequest,
};

/// Node-local registry of independently addressed replica stores.
///
/// It never discovers or claims devices. The server constructs it only from
/// explicit configuration, then the data plane routes every operation by the
/// committed [`DeviceId`].
pub struct DeviceStore {
    default_device: DeviceId,
    stores: BTreeMap<DeviceId, Arc<dyn ReplicaStore>>,
}

impl DeviceStore {
    /// Creates a single-device compatibility registry.
    #[must_use]
    pub fn single(device_id: DeviceId, store: Arc<dyn ReplicaStore>) -> Self {
        Self {
            default_device: device_id,
            stores: BTreeMap::from([(device_id, store)]),
        }
    }

    /// Creates a validated multi-device registry.
    pub fn new(
        default_device: DeviceId,
        stores: impl IntoIterator<Item = (DeviceId, Arc<dyn ReplicaStore>)>,
    ) -> Result<Self, StorageError> {
        let stores: BTreeMap<_, _> = stores.into_iter().collect();
        if !stores.contains_key(&default_device) {
            return Err(StorageError::UnknownDevice(default_device));
        }
        Ok(Self {
            default_device,
            stores,
        })
    }

    /// Returns the compatibility/default device identity.
    #[must_use]
    pub const fn default_device_id(&self) -> DeviceId {
        self.default_device
    }

    /// Resolves an exact local store.
    pub fn for_device(&self, device_id: DeviceId) -> Result<Arc<dyn ReplicaStore>, StorageError> {
        self.stores
            .get(&device_id)
            .cloned()
            .ok_or(StorageError::UnknownDevice(device_id))
    }

    /// Returns configured device identities in stable order.
    pub fn device_ids(&self) -> impl Iterator<Item = DeviceId> + '_ {
        self.stores.keys().copied()
    }

    /// Returns the number of configured devices.
    #[must_use]
    pub fn len(&self) -> usize {
        self.stores.len()
    }

    /// Returns whether no device is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.stores.is_empty()
    }

    /// Sums capacity across every configured device.
    ///
    /// Node-wide questions — heartbeats, readiness, operator status — are about
    /// the node, not about whichever device happens to be the default. Devices
    /// are registered as independent resources, so their capacities add; two
    /// device roots sharing one filesystem would be a misconfiguration that this
    /// cannot detect and would double-count.
    pub async fn capacity(&self) -> Result<StorageStatus, StorageError> {
        let mut total = StorageStatus {
            capacity_bytes: 0,
            available_bytes: 0,
            temporary_upload_bytes: 0,
        };
        for store in self.stores.values() {
            let status = store.local_capacity().await?;
            total.capacity_bytes = total.capacity_bytes.saturating_add(status.capacity_bytes);
            total.available_bytes = total.available_bytes.saturating_add(status.available_bytes);
            total.temporary_upload_bytes = total
                .temporary_upload_bytes
                .saturating_add(status.temporary_upload_bytes);
        }
        Ok(total)
    }

    /// Lists payload identifiers held anywhere on this node.
    ///
    /// Reconciliation and inspection walk what the node actually stores, which
    /// spans devices. Identifiers are merged into one ordered, de-duplicated
    /// page so a caller cannot miss a payload that moved between devices.
    pub async fn list_payloads(
        &self,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<Vec<ObjectId>, StorageError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut merged = BTreeSet::new();
        for store in self.stores.values() {
            merged.extend(store.list_local_payloads(after, limit).await?);
        }
        Ok(merged.into_iter().take(limit).collect())
    }

    /// Finds the device holding a payload.
    ///
    /// Returns the first device in stable order that has it, so the answer does
    /// not depend on map iteration order.
    pub async fn locate(
        &self,
        object_id: ObjectId,
    ) -> Result<Option<(DeviceId, ReplicaStat)>, StorageError> {
        for (device_id, store) in &self.stores {
            if let Some(stat) = store.stat_replica(object_id).await? {
                return Ok(Some((*device_id, stat)));
            }
        }
        Ok(None)
    }

    /// Removes a payload from every device holding it.
    ///
    /// Deletion is a node-wide instruction: a tombstone must not leave bytes
    /// behind on a device the caller did not think to name. Returns whether any
    /// device held the payload.
    pub async fn delete_everywhere(&self, object_id: ObjectId) -> Result<bool, StorageError> {
        let mut removed = false;
        for store in self.stores.values() {
            removed |= store.delete_replica(object_id).await?;
        }
        Ok(removed)
    }
}

// Compatibility operations target the configured default. Cluster paths use
// `for_device` explicitly; keeping this implementation preserves existing
// single-device maintenance code and standalone semantics.
#[async_trait]
impl ReplicaStore for DeviceStore {
    async fn write_replica(
        &self,
        request: WriteReplicaRequest,
    ) -> Result<ReplicaWriteResult, StorageError> {
        self.for_device(self.default_device)?
            .write_replica(request)
            .await
    }

    async fn read_replica(
        &self,
        request: ReadReplicaRequest,
    ) -> Result<ReplicaReadResult, StorageError> {
        self.for_device(self.default_device)?
            .read_replica(request)
            .await
    }

    async fn delete_replica(&self, object_id: ObjectId) -> Result<bool, StorageError> {
        self.for_device(self.default_device)?
            .delete_replica(object_id)
            .await
    }

    async fn verify_replica(
        &self,
        object_id: ObjectId,
        size: u64,
        payload_format: PayloadFormat,
        expected: Checksum,
    ) -> Result<ReplicaVerification, StorageError> {
        self.for_device(self.default_device)?
            .verify_replica(object_id, size, payload_format, expected)
            .await
    }

    async fn stat_replica(&self, object_id: ObjectId) -> Result<Option<ReplicaStat>, StorageError> {
        self.for_device(self.default_device)?
            .stat_replica(object_id)
            .await
    }

    async fn list_local_payloads(
        &self,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<Vec<ObjectId>, StorageError> {
        self.for_device(self.default_device)?
            .list_local_payloads(after, limit)
            .await
    }

    async fn local_capacity(&self) -> Result<StorageStatus, StorageError> {
        self.for_device(self.default_device)?.local_capacity().await
    }
}
