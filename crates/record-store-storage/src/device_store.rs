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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use record_store_core::{Checksum, DeviceId, ObjectId};
    use record_store_metadata::{MetadataRepository, RedbMetadataRepository};

    use super::*;
    use crate::{LocalFilesystemStore, upload_stream};

    /// Two devices backed by two directories, as a node with two drives has.
    async fn two_devices() -> (tempfile::TempDir, DeviceStore, DeviceId, DeviceId) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let metadata: Arc<dyn MetadataRepository> = Arc::new(
            RedbMetadataRepository::open(directory.path().join("metadata.redb"))
                .await
                .expect("metadata repository"),
        );

        let mut stores: Vec<(DeviceId, Arc<dyn ReplicaStore>)> = Vec::new();
        let mut ids = Vec::new();
        for name in ["disk-a", "disk-b"] {
            let root = directory.path().join(name);
            let store = LocalFilesystemStore::open(&root, root.join("tmp"), Arc::clone(&metadata))
                .await
                .expect("replica store");
            let id = DeviceId::new();
            ids.push(id);
            stores.push((id, Arc::new(store) as Arc<dyn ReplicaStore>));
        }

        let registry = DeviceStore::new(ids[0], stores).expect("registry");
        (directory, registry, ids[0], ids[1])
    }

    async fn write(registry: &DeviceStore, device: DeviceId, object_id: ObjectId, body: &[u8]) {
        let payload = body.to_vec();
        registry
            .for_device(device)
            .expect("device")
            .write_replica(WriteReplicaRequest::known(
                format!("test-{object_id}"),
                object_id,
                payload.len() as u64,
                Checksum::sha256(<[u8; 32]>::from(<sha2::Sha256 as sha2::Digest>::digest(
                    &payload,
                ))),
                upload_stream(futures_util::stream::once(async move {
                    Ok(bytes::Bytes::from(payload))
                })),
            ))
            .await
            .expect("write");
    }

    /// A registry that names a device it has no store for would route writes
    /// into nothing, so it is refused at construction.
    #[tokio::test]
    async fn a_registry_must_hold_the_device_it_defaults_to() {
        let (_directory, registry, first, second) = two_devices().await;
        assert_eq!(registry.len(), 2);
        // Identifiers come back in stable sorted order, not insertion order, so
        // every node iterating a registry sees the same sequence.
        let mut expected = vec![first, second];
        expected.sort_unstable();
        assert_eq!(registry.device_ids().collect::<Vec<_>>(), expected);

        let absent = DeviceId::new();
        assert!(matches!(
            registry.for_device(absent),
            Err(StorageError::UnknownDevice(_))
        ));
    }

    /// Capacity is a question about the node, not about whichever device is
    /// nominated as default.
    ///
    /// Both devices here share one filesystem, so the sum double-counts it. That
    /// is the documented assumption — registered devices are independent
    /// resources — and pinning it here keeps the limitation visible rather than
    /// leaving it as a surprise for whoever puts two device roots on one mount.
    #[tokio::test]
    async fn capacity_sums_across_devices() {
        let (_directory, registry, first, _second) = two_devices().await;

        let one = registry
            .for_device(first)
            .expect("device")
            .local_capacity()
            .await
            .expect("capacity");
        let all = registry.capacity().await.expect("capacity");
        assert_eq!(
            all.capacity_bytes,
            one.capacity_bytes * 2,
            "capacity adds per registered device"
        );
    }

    /// Reconciliation walks what the node stores, which spans drives. Missing a
    /// payload here would report a healthy replica as lost and repair it
    /// needlessly.
    #[tokio::test]
    async fn listing_and_locating_span_every_device() {
        let (_directory, registry, first, second) = two_devices().await;
        let on_first = ObjectId::new();
        let on_second = ObjectId::new();
        write(&registry, first, on_first, b"first").await;
        write(&registry, second, on_second, b"second").await;

        let listed = registry.list_payloads(None, 100).await.expect("list");
        assert!(
            listed.contains(&on_first),
            "a payload on the default device is listed"
        );
        assert!(
            listed.contains(&on_second),
            "a payload on a non-default device must not be invisible"
        );

        let (located, _) = registry
            .locate(on_second)
            .await
            .expect("locate")
            .expect("the payload exists");
        assert_eq!(
            located, second,
            "located on the device that actually holds it"
        );
        assert!(
            registry
                .locate(ObjectId::new())
                .await
                .expect("locate")
                .is_none(),
            "a payload nobody stored is not found somewhere"
        );
    }

    /// A tombstone is a node-wide instruction. Deleting only from the default
    /// device would leave bytes behind on a drive nobody named.
    #[tokio::test]
    async fn deletion_reaches_every_device_holding_the_payload() {
        let (_directory, registry, first, second) = two_devices().await;
        // The same payload on both drives, as a repair mid-flight can leave it.
        let object_id = ObjectId::new();
        write(&registry, first, object_id, b"copy").await;
        write(&registry, second, object_id, b"copy").await;

        assert!(registry.delete_everywhere(object_id).await.expect("delete"));
        assert!(
            registry.locate(object_id).await.expect("locate").is_none(),
            "bytes survived on a device the caller did not name"
        );
        assert!(
            !registry.delete_everywhere(object_id).await.expect("delete"),
            "deleting what is already gone reports no removal, and does not fail"
        );
    }

    /// Compatibility routing targets the default device, which is what keeps
    /// standalone and single-drive deployments behaving exactly as before.
    #[tokio::test]
    async fn unqualified_operations_use_the_default_device() {
        let (_directory, registry, first, _second) = two_devices().await;
        let object_id = ObjectId::new();
        write(&registry, first, object_id, b"payload").await;

        let stat = registry.stat_replica(object_id).await.expect("stat");
        assert!(
            stat.is_some(),
            "the default device answers unqualified reads"
        );
        assert_eq!(registry.default_device_id(), first);
    }
}
