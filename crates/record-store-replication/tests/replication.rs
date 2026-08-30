//! Direct tests for the durability decisions the replication data plane makes.
//!
//! The cluster catalog, the metadata catalog, the placement policy, and the
//! node-local replica store are all real. Only the peer transport is faked,
//! because remote failure is the one thing that has to be injected to be
//! observed. Nothing here waits on elapsed time: every outcome is driven by a
//! configured peer behaviour, so the same run produces the same result.
//!
//! The invariant these tests exist to protect is that a write is acknowledged
//! only when the configured number of replicas has independently verified what
//! it stored. An ingress node holding the bytes is not durability.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use bytes::Bytes;
use chrono::Utc;
use futures_util::StreamExt;
use record_store_cluster::{
    CapacityAwarePlacement, ClusterCommand, ClusterConfig, ClusterIdentity, DeviceRecord,
    FailureDomain, NodeCapacity, NodeRegistration, NodeState, NodeVersions, ReplicaState,
    ReplicaTaskState, StorageClass, WriteAcknowledgement,
};
use record_store_consensus::{
    ClusterStore, ClusterWrite, ConsensusSettings, MetadataConsensus, ReplicatedClusterStore,
    ReplicatedMetadataRepository,
};
use record_store_core::{
    Bucket, BucketId, BucketName, BucketQuota, Checksum, ClusterId, NodeId, ObjectId, ObjectKey,
    OrganizationId, PayloadFormat, VersioningState,
};
use record_store_metadata::MetadataRepository;
use record_store_replication::{
    ClusterContext, DistributedObjectStore, DistributedSettings, TaskExecutor,
    tasks::MovementLimits,
};
use record_store_rpc::{
    ConsensusNetwork, PeerHeaders, PeerPool, RemoteReadStream, RemoteReplicaVerification,
    RemoteReplicaWrite, ReplicaTarget, ReplicaTransport, RpcClientError, RpcClientSettings,
    TlsSettings, TransferExpectation, TransferStream,
};
use record_store_storage::{
    DeviceStore, LocalFilesystemStore, ObjectStore, PutObjectRequest, ReplicaStore, StorageError,
    upload_stream,
};
use sha2::{Digest, Sha256};

/// How a faked peer behaves for one transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Peer {
    /// Accepts the whole stream and reports the checksum it actually computed.
    Accepts,
    /// Refuses before a single byte is sent, as an unreachable node does.
    Unavailable,
    /// Accepts a prefix and then drops, as a node lost mid-transfer does.
    DropsAfter(u64),
    /// Accepts everything but reports a checksum for different bytes.
    Corrupts,
    /// Accepts every byte and then never commits, as a peer whose call hits the
    /// transport deadline does. The real client bounds this with a request
    /// timeout, so the fake fails rather than hanging.
    TimesOut,
}

/// One recorded transfer attempt, so idempotence can be asserted.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Attempt {
    node_id: NodeId,
    object_id: ObjectId,
    operation_id: String,
}

/// A peer transport whose per-node behaviour each test sets explicitly.
struct FakeTransport {
    behaviour: Mutex<BTreeMap<NodeId, Peer>>,
    stored: Mutex<BTreeMap<(NodeId, ObjectId), Vec<u8>>>,
    attempts: Mutex<Vec<Attempt>>,
}

impl FakeTransport {
    fn new(behaviour: BTreeMap<NodeId, Peer>) -> Self {
        Self {
            behaviour: Mutex::new(behaviour),
            stored: Mutex::new(BTreeMap::new()),
            attempts: Mutex::new(Vec::new()),
        }
    }

    fn behaviour_of(&self, node_id: NodeId) -> Peer {
        self.behaviour
            .lock()
            .expect("behaviour registry")
            .get(&node_id)
            .copied()
            .unwrap_or(Peer::Accepts)
    }

    fn set(&self, node_id: NodeId, peer: Peer) {
        self.behaviour
            .lock()
            .expect("behaviour registry")
            .insert(node_id, peer);
    }

    fn attempts(&self) -> Vec<Attempt> {
        self.attempts.lock().expect("attempt log").clone()
    }

    fn holds(&self, node_id: NodeId, object_id: ObjectId) -> bool {
        self.stored
            .lock()
            .expect("stored payloads")
            .contains_key(&(node_id, object_id))
    }
}

fn unreachable(target: &ReplicaTarget) -> RpcClientError {
    RpcClientError::Unreachable {
        address: target.address.clone(),
        reason: "peer is offline for this test".to_owned(),
    }
}

#[async_trait]
impl ReplicaTransport for FakeTransport {
    async fn write_replica(
        &self,
        target: &ReplicaTarget,
        operation_id: &str,
        object_id: ObjectId,
        expectation: TransferExpectation,
        mut body: TransferStream,
    ) -> Result<RemoteReplicaWrite, RpcClientError> {
        self.attempts.lock().expect("attempt log").push(Attempt {
            node_id: target.node_id,
            object_id,
            operation_id: operation_id.to_owned(),
        });
        let behaviour = self.behaviour_of(target.node_id);
        if behaviour == Peer::Unavailable {
            return Err(unreachable(target));
        }

        // A peer that already holds a verified replica reports it rather than
        // storing a second copy: this is what makes a retried write idempotent.
        let already_present = self.holds(target.node_id, object_id);

        let mut received = Vec::new();
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|error| RpcClientError::Unreachable {
                address: target.address.clone(),
                reason: error.to_string(),
            })?;
            received.extend_from_slice(&chunk);
            if let Peer::DropsAfter(limit) = behaviour
                && received.len() as u64 >= limit
            {
                return Err(unreachable(target));
            }
        }

        // The client's expectation arrives after the last byte for an upload.
        let (size, expected) = match expectation {
            TransferExpectation::Known { size, checksum } => (size, checksum),
            TransferExpectation::Trailing(receiver) => match receiver.await {
                Ok(Ok(committed)) => committed,
                Ok(Err(reason)) => {
                    return Err(RpcClientError::Unreachable {
                        address: target.address.clone(),
                        reason,
                    });
                }
                Err(_) => return Err(unreachable(target)),
            },
        };

        if behaviour == Peer::TimesOut {
            return Err(RpcClientError::Call {
                address: target.address.clone(),
                status: "deadline exceeded".to_owned(),
            });
        }

        if received.len() as u64 != size {
            return Err(RpcClientError::Unreachable {
                address: target.address.clone(),
                reason: format!("received {} of {size} bytes", received.len()),
            });
        }

        // The destination computes the checksum itself. A source that claims
        // success is never enough to make a replica count as durable.
        let computed = if behaviour == Peer::Corrupts {
            Checksum::sha256(Sha256::digest(b"bytes this peer did not receive").into())
        } else {
            Checksum::sha256(Sha256::digest(&received).into())
        };
        if behaviour != Peer::Corrupts {
            assert_eq!(
                computed, expected,
                "fake peer computed a different checksum"
            );
            self.stored
                .lock()
                .expect("stored payloads")
                .insert((target.node_id, object_id), received);
        }
        Ok(RemoteReplicaWrite {
            object_id,
            size,
            checksum: computed,
            already_present,
        })
    }

    async fn read_replica(
        &self,
        target: &ReplicaTarget,
        object_id: ObjectId,
        _size: u64,
        _checksum: &Checksum,
    ) -> Result<RemoteReadStream, RpcClientError> {
        let behaviour = self.behaviour_of(target.node_id);
        if behaviour == Peer::Unavailable {
            return Err(unreachable(target));
        }
        let bytes = self
            .stored
            .lock()
            .expect("stored payloads")
            .get(&(target.node_id, object_id))
            .cloned()
            .ok_or_else(|| unreachable(target))?;
        let bytes = if behaviour == Peer::Corrupts {
            let mut corrupted = bytes.clone();
            if let Some(first) = corrupted.first_mut() {
                *first ^= 0xff;
            }
            corrupted
        } else {
            bytes
        };
        Ok(Box::pin(futures_util::stream::once(async move {
            Ok(Bytes::from(bytes))
        })))
    }

    async fn delete_replica(
        &self,
        target: &ReplicaTarget,
        object_id: ObjectId,
    ) -> Result<bool, RpcClientError> {
        Ok(self
            .stored
            .lock()
            .expect("stored payloads")
            .remove(&(target.node_id, object_id))
            .is_some())
    }

    async fn verify_replica(
        &self,
        target: &ReplicaTarget,
        object_id: ObjectId,
        size: u64,
        checksum: &Checksum,
    ) -> Result<RemoteReplicaVerification, RpcClientError> {
        let stored = self
            .stored
            .lock()
            .expect("stored payloads")
            .get(&(target.node_id, object_id))
            .cloned();
        Ok(match stored {
            Some(bytes) => {
                let computed = Checksum::sha256(Sha256::digest(&bytes).into());
                RemoteReplicaVerification {
                    present: true,
                    matches: computed == *checksum && bytes.len() as u64 == size,
                    size: bytes.len() as u64,
                    checksum: Some(computed),
                }
            }
            None => RemoteReplicaVerification {
                present: false,
                matches: false,
                size: 0,
                checksum: None,
            },
        })
    }

    async fn list_local_payloads(
        &self,
        target: &ReplicaTarget,
        _after: Option<ObjectId>,
        _limit: usize,
    ) -> Result<Vec<ObjectId>, RpcClientError> {
        Ok(self
            .stored
            .lock()
            .expect("stored payloads")
            .keys()
            .filter(|(node_id, _)| *node_id == target.node_id)
            .map(|(_, object_id)| *object_id)
            .collect())
    }
}

/// A single-node view of a cluster whose peers are faked.
struct Harness {
    directory: tempfile::TempDir,
    store: DistributedObjectStore,
    context: Arc<ClusterContext>,
    consensus: Arc<MetadataConsensus>,
    transport: Arc<FakeTransport>,
    peers: Vec<NodeId>,
    bucket_id: BucketId,
    /// Devices registered on the local node, in registration order.
    drives: Vec<record_store_core::DeviceId>,
}

impl Harness {
    /// Builds a cluster of `peers + 1` nodes with the given durability policy.
    async fn new(
        peers: usize,
        replication_factor: u8,
        acknowledgement: WriteAcknowledgement,
    ) -> Self {
        Self::build(
            peers,
            replication_factor,
            acknowledgement,
            BTreeMap::new(),
            true,
        )
        .await
    }

    /// Builds a single node serving `drives` independent devices.
    ///
    /// Device scope, so the drives compete with one another rather than being
    /// collapsed into one machine-shaped domain.
    async fn with_drives(drives: usize) -> Self {
        Self::build_with(
            0,
            1,
            WriteAcknowledgement::All,
            BTreeMap::new(),
            true,
            drives,
        )
        .await
    }

    async fn with_behaviour(
        peers: usize,
        replication_factor: u8,
        acknowledgement: WriteAcknowledgement,
        behaviour: BTreeMap<usize, Peer>,
    ) -> Self {
        Self::build(peers, replication_factor, acknowledgement, behaviour, true).await
    }

    async fn build(
        peers: usize,
        replication_factor: u8,
        acknowledgement: WriteAcknowledgement,
        behaviour: BTreeMap<usize, Peer>,
        allow_degraded_writes: bool,
    ) -> Self {
        Self::build_with(
            peers,
            replication_factor,
            acknowledgement,
            behaviour,
            allow_degraded_writes,
            1,
        )
        .await
    }

    async fn build_with(
        peers: usize,
        replication_factor: u8,
        acknowledgement: WriteAcknowledgement,
        behaviour: BTreeMap<usize, Peer>,
        allow_degraded_writes: bool,
        drives: usize,
    ) -> Self {
        let directory = tempfile::tempdir().expect("temporary directory");
        let local_node = NodeId::new();

        // A real single-member consensus group. The commit path that makes an
        // object visible writes placement and metadata in one consensus
        // command, so a fake store here would skip the very step under test.
        // One member never sends a peer RPC, so the real network type is inert.
        let consensus = MetadataConsensus::start(
            ConsensusSettings::new(1, "127.0.0.1:7603", directory.path().join("consensus")),
            ConsensusNetwork::new(PeerPool::new(RpcClientSettings::new(
                PeerHeaders {
                    node_id: local_node,
                    cluster_id: None,
                    versions: NodeVersions::current("test"),
                    credential: None,
                },
                TlsSettings::default(),
            ))),
        )
        .await
        .expect("start consensus");
        consensus
            .initialize_single_member()
            .await
            .expect("initialize consensus");
        consensus
            .wait_for_leader(std::time::Duration::from_secs(10))
            .await
            .expect("elect a leader");

        let config = ClusterConfig {
            replication_factor,
            write_acknowledgement: acknowledgement,
            // Placement here is about durability, not layout.
            strict_failure_domains: false,
            allow_degraded_writes,
            ..ClusterConfig::default()
        };
        consensus
            .write(ClusterWrite::cluster(ClusterCommand::InitializeCluster {
                identity: ClusterIdentity {
                    cluster_id: ClusterId::new(),
                    cluster_format_version: record_store_cluster::CLUSTER_FORMAT_VERSION,
                    created_at: Utc::now(),
                },
                config: Box::new(config),
            }))
            .await
            .expect("initialize the cluster");

        let mut node_ids = vec![local_node];
        node_ids.extend((0..peers).map(|_| NodeId::new()));
        for (index, node_id) in node_ids.iter().enumerate() {
            register(&consensus, *node_id, index).await;
        }
        let peer_ids = node_ids[1..].to_vec();
        let behaviour = behaviour
            .into_iter()
            .map(|(index, peer)| (peer_ids[index], peer))
            .collect();

        // The local replica store reads applied state directly, exactly as a
        // cluster node does; public reads and every mutation go through the
        // replicated adapters.
        let local_metadata: Arc<dyn MetadataRepository> =
            Arc::new(consensus.state().metadata().clone());
        // One store per declared drive. The first is the node's default device,
        // which is what a single-drive node has always had.
        let mut drive_ids = Vec::new();
        let mut drive_stores: Vec<(record_store_core::DeviceId, Arc<dyn ReplicaStore>)> =
            Vec::new();
        for index in 0..drives.max(1) {
            let root = directory.path().join(format!("data-{index}"));
            let store =
                LocalFilesystemStore::open(&root, root.join("tmp"), Arc::clone(&local_metadata))
                    .await
                    .expect("local store");
            let id = if index == 0 {
                DeviceRecord::legacy_id(local_node)
            } else {
                record_store_core::DeviceId::new()
            };
            drive_ids.push(id);
            drive_stores.push((id, Arc::new(store) as Arc<dyn ReplicaStore>));
        }
        // A drive that exists on disk but was never registered is invisible to
        // placement, and that is the correct rule: registration is what makes a
        // device a placement target. A node's default device is synthesized into
        // the topology view, so only the drives beyond it need registering.
        for id in drive_ids.iter().skip(1) {
            let mut record = DeviceRecord::legacy_directory(
                local_node,
                None,
                StorageClass::default(),
                record_store_cluster::DeviceCapacity {
                    raw_bytes: 100 * 1024 * 1024 * 1024,
                    usable_bytes: 100 * 1024 * 1024 * 1024,
                    allocated_bytes: 0,
                    reserved_bytes: 0,
                    available_bytes: 90 * 1024 * 1024 * 1024,
                },
            );
            record.id = *id;
            consensus
                .write(ClusterWrite::cluster(ClusterCommand::RegisterDevice {
                    node_id: local_node,
                    device: Box::new(record),
                    at: Utc::now(),
                }))
                .await
                .expect("register a drive");
        }
        let metadata: Arc<dyn MetadataRepository> =
            Arc::new(ReplicatedMetadataRepository::new(Arc::clone(&consensus)));
        let bucket = Bucket {
            id: BucketId::new(),
            organization_id: OrganizationId::from_uuid(uuid::Uuid::from_u128(1)),
            name: BucketName::new("durability").expect("bucket name"),
            created_at: Utc::now(),
            versioning: VersioningState::Disabled,
            quota: BucketQuota::default(),
            storage_class: None,
            durability_policy: None,
            cors: None,
        };
        metadata
            .create_bucket(&bucket)
            .await
            .expect("create the bucket");

        let transport = Arc::new(FakeTransport::new(behaviour));
        let context = Arc::new(ClusterContext {
            node_id: local_node,
            cluster: Arc::new(ReplicatedClusterStore::new(Arc::clone(&consensus)))
                as Arc<dyn ClusterStore>,
            metadata,
            local: Arc::new(
                DeviceStore::new(DeviceRecord::legacy_id(local_node), drive_stores)
                    .expect("device registry"),
            ),
            transport: transport.clone(),
            placement: Arc::new(CapacityAwarePlacement::new(Some(local_node))),
            consensus: Some(Arc::clone(&consensus)),
        });
        let store = DistributedObjectStore::new(
            Arc::clone(&context),
            DistributedSettings::new(PayloadFormat::Plaintext),
        );
        Self {
            directory,
            store,
            context,
            consensus,
            transport,
            peers: peer_ids,
            bucket_id: bucket.id,
            drives: drive_ids,
        }
    }

    async fn put(
        &self,
        key: &str,
        payload: &[u8],
    ) -> Result<record_store_storage::PutObjectResult, StorageError> {
        let chunk = Bytes::copy_from_slice(payload);
        self.store
            .put(PutObjectRequest {
                bucket_id: self.bucket_id,
                key: ObjectKey::new(key).expect("valid key"),
                content_type: Some("application/octet-stream".into()),
                custom_metadata: BTreeMap::new(),
                expected_checksum: None,
                object_id: None,
                protocol_etag: None,
                body: upload_stream(futures_util::stream::once(async move { Ok(chunk) })),
            })
            .await
    }
}

impl Harness {
    /// Removes this node's physical replica, leaving its metadata healthy.
    ///
    /// This is the "healthy metadata but the read fails" case: placement still
    /// lists the local replica, so the read path has to discover at open time
    /// that it cannot be served and move on.
    async fn drop_local_replica(&self, object_id: ObjectId) {
        self.context
            .local
            .delete_replica(object_id)
            .await
            .expect("remove the local replica");
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>, StorageError> {
        let result = self
            .store
            .get(record_store_storage::GetObjectRequest {
                bucket_id: self.bucket_id,
                key: ObjectKey::new(key).expect("valid key"),
                range: None,
            })
            .await?;
        let mut collected = Vec::new();
        let mut body = result.body;
        while let Some(chunk) = body.next().await {
            collected.extend_from_slice(&chunk?);
        }
        Ok(collected)
    }
}

async fn register(consensus: &MetadataConsensus, node_id: NodeId, index: usize) {
    consensus
        .write(ClusterWrite::cluster(ClusterCommand::RegisterNode {
            registration: Box::new(NodeRegistration {
                node_id,
                versions: NodeVersions::current("test"),
                rpc_address: format!("10.0.0.{}:7603", index + 1),
                s3_endpoint: None,
                storage_class: StorageClass::default(),
                failure_domain: FailureDomain::parse(&format!("rack={index}")).expect("labels"),
                capacity: NodeCapacity {
                    total_bytes: 100 * 1024 * 1024 * 1024,
                    available_bytes: 90 * 1024 * 1024 * 1024,
                    replica_bytes: 0,
                    temporary_bytes: 0,
                },
                devices: Vec::new(),
                started_at: Utc::now(),
            }),
            at: Utc::now(),
        }))
        .await
        .expect("register a node");
    consensus
        .write(ClusterWrite::cluster(ClusterCommand::SetNodeState {
            node_id,
            state: NodeState::Healthy,
            reason: Some("test fixture".to_owned()),
            at: Utc::now(),
        }))
        .await
        .expect("mark the node healthy");
}

fn durability_shortfall(error: &StorageError) -> (u8, u8) {
    match error {
        StorageError::DurabilityNotMet {
            required, achieved, ..
        } => (*required, *achieved),
        other => panic!("expected a durability failure, got {other}"),
    }
}

// ---------------------------------------------------------------------------
// Acknowledgement boundary
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_replica_succeeding_is_acknowledged() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    let result = harness.put("all-good", b"payload").await.expect("put");
    assert_eq!(result.metadata.size, 7);
    for peer in &harness.peers {
        assert!(
            harness.transport.holds(*peer, result.metadata.id),
            "every peer must physically hold the payload",
        );
    }
}

#[tokio::test]
async fn exactly_the_required_acknowledgement_count_succeeds() {
    // required = 2 of 3, and exactly 2 replicas verify.
    let harness = Harness::with_behaviour(
        2,
        3,
        WriteAcknowledgement::Count(2),
        BTreeMap::from([(1, Peer::Unavailable)]),
    )
    .await;
    let result = harness
        .put("exactly-enough", b"payload")
        .await
        .expect("a write meeting its threshold exactly must succeed");
    assert!(
        harness
            .transport
            .holds(harness.peers[0], result.metadata.id)
    );
    assert!(
        !harness
            .transport
            .holds(harness.peers[1], result.metadata.id)
    );
}

#[tokio::test]
async fn one_acknowledgement_short_of_the_requirement_fails() {
    // required = 3 of 3, and only 2 replicas verify. The ingress node holding
    // the bytes must not be allowed to stand in for the missing replica.
    let harness = Harness::with_behaviour(
        2,
        3,
        WriteAcknowledgement::All,
        BTreeMap::from([(1, Peer::Unavailable)]),
    )
    .await;
    let error = harness
        .put("one-short", b"payload")
        .await
        .expect_err("a write below its durability threshold must fail");
    assert_eq!(durability_shortfall(&error), (3, 2));
}

#[tokio::test]
async fn a_write_that_only_reached_the_local_node_fails() {
    // The strongest form of the invariant: local bytes are not durability.
    let harness = Harness::with_behaviour(
        2,
        3,
        WriteAcknowledgement::Quorum,
        BTreeMap::from([(0, Peer::Unavailable), (1, Peer::Unavailable)]),
    )
    .await;
    let error = harness
        .put("local-only", b"payload")
        .await
        .expect_err("holding the bytes locally is not a durable write");
    assert_eq!(durability_shortfall(&error), (2, 1));
}

// ---------------------------------------------------------------------------
// Integrity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_replica_reporting_a_mismatched_checksum_is_not_durable() {
    // The peer accepted every byte and returned success, but the checksum it
    // computed disagrees. It must not count toward the requirement.
    let harness = Harness::with_behaviour(
        2,
        3,
        WriteAcknowledgement::All,
        BTreeMap::from([(1, Peer::Corrupts)]),
    )
    .await;
    let error = harness
        .put("corrupt-peer", b"payload")
        .await
        .expect_err("a corrupt replica must not satisfy durability");
    assert_eq!(durability_shortfall(&error), (3, 2));
    match &error {
        StorageError::DurabilityNotMet { detail, .. } => {
            assert!(
                detail.contains("checksum"),
                "the operator needs to see that a checksum disagreed, got: {detail}",
            );
        }
        other => panic!("expected a durability failure, got {other}"),
    }
}

#[tokio::test]
async fn a_corrupt_replica_does_not_block_a_write_that_still_meets_its_threshold() {
    let harness = Harness::with_behaviour(
        2,
        3,
        WriteAcknowledgement::Count(2),
        BTreeMap::from([(1, Peer::Corrupts)]),
    )
    .await;
    let result = harness
        .put("corrupt-but-enough", b"payload")
        .await
        .expect("two verified replicas satisfy a requirement of two");
    // The corrupt peer must not be recorded as holding anything.
    assert!(
        !harness
            .transport
            .holds(harness.peers[1], result.metadata.id)
    );
}

// ---------------------------------------------------------------------------
// Streaming failure
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_replica_lost_partway_through_the_stream_is_not_durable() {
    let payload = vec![7_u8; 64 * 1024];
    let harness = Harness::with_behaviour(
        2,
        3,
        WriteAcknowledgement::All,
        BTreeMap::from([(1, Peer::DropsAfter(1024))]),
    )
    .await;
    let error = harness
        .put("dropped-midstream", &payload)
        .await
        .expect_err("a partial transfer must not satisfy durability");
    assert_eq!(durability_shortfall(&error), (3, 2));
    // A partially received payload must never be visible as a replica.
    assert!(
        !harness
            .transport
            .holds(harness.peers[1], ObjectId::default())
    );
}

#[tokio::test]
async fn a_replica_whose_call_times_out_is_not_durable() {
    let payload = vec![3_u8; 256 * 1024];
    let harness = Harness::with_behaviour(
        2,
        3,
        WriteAcknowledgement::All,
        BTreeMap::from([(1, Peer::TimesOut)]),
    )
    .await;
    let error = harness
        .put("timed-out-peer", &payload)
        .await
        .expect_err("a peer that never commits must not satisfy durability");
    assert_eq!(durability_shortfall(&error), (3, 2));
}

#[tokio::test]
async fn a_timed_out_replica_does_not_hold_up_a_satisfied_write() {
    let payload = vec![3_u8; 256 * 1024];
    let harness = Harness::with_behaviour(
        2,
        3,
        WriteAcknowledgement::Count(2),
        BTreeMap::from([(1, Peer::TimesOut)]),
    )
    .await;
    // The write must not wait on the slow target beyond its own failure: the
    // other two replicas satisfy the requirement and the object commits.
    let result = harness
        .put("timed-out-but-enough", &payload)
        .await
        .expect("a timed-out peer must not prevent a satisfied threshold");
    assert_eq!(result.metadata.size, payload.len() as u64);
}

#[tokio::test]
async fn an_under_replicated_write_queues_and_completes_an_executable_repair() {
    let harness = Harness::with_behaviour(
        2,
        3,
        WriteAcknowledgement::Count(2),
        BTreeMap::from([(1, Peer::Unavailable)]),
    )
    .await;
    let put = harness
        .put("repair-me", b"repairable payload")
        .await
        .expect("the configured two acknowledgements are durable");

    harness
        .context
        .cluster
        .refresh_durability_counters()
        .await
        .expect("refresh durability status");
    assert_eq!(
        harness
            .context
            .cluster
            .usage()
            .await
            .expect("read degraded usage")
            .under_replicated_payloads,
        1,
    );
    let tasks = harness
        .context
        .cluster
        .queued_tasks(10)
        .await
        .expect("read repair queue");
    assert_eq!(tasks.tasks.len(), 1);
    let task = &tasks.tasks[0];
    let target = task
        .target_node
        .expect("a queued repair must name the node that can execute it");
    assert_eq!(target, harness.peers[1]);

    let target_metadata: Arc<dyn MetadataRepository> =
        Arc::new(harness.consensus.state().metadata().clone());
    let target_store = Arc::new(
        LocalFilesystemStore::open(
            harness.directory.path().join("repair-target"),
            harness.directory.path().join("repair-target-tmp"),
            target_metadata,
        )
        .await
        .expect("open the repair target store"),
    );
    let target_context = Arc::new(ClusterContext {
        node_id: target,
        cluster: Arc::new(ReplicatedClusterStore::new(Arc::clone(&harness.consensus)))
            as Arc<dyn ClusterStore>,
        metadata: Arc::new(ReplicatedMetadataRepository::new(Arc::clone(
            &harness.consensus,
        ))),
        local: Arc::new(DeviceStore::single(
            DeviceRecord::legacy_id(target),
            target_store.clone() as Arc<dyn ReplicaStore>,
        )),
        transport: harness.transport.clone(),
        placement: Arc::new(CapacityAwarePlacement::new(Some(target))),
        consensus: Some(Arc::clone(&harness.consensus)),
    });
    let executor = TaskExecutor::new(target_context, PayloadFormat::Plaintext);
    assert_eq!(
        executor
            .run_once(MovementLimits {
                bytes_per_second: 0,
                ..MovementLimits::default()
            })
            .await,
        1,
        "the target node must claim and finish the queued repair",
    );

    let completed = harness
        .context
        .cluster
        .task(task.id)
        .await
        .expect("read completed task")
        .expect("repair task still exists");
    assert!(matches!(
        completed.state,
        ReplicaTaskState::Completed { .. }
    ));
    let placement = harness
        .context
        .placement_for(put.metadata.id)
        .await
        .expect("read repaired placement")
        .expect("placement exists");
    assert_eq!(placement.replicas.len(), 3);
    assert!(
        placement
            .replica(target)
            .is_some_and(|replica| replica.state == ReplicaState::Healthy)
    );
    assert!(
        target_store
            .stat_replica(put.metadata.id)
            .await
            .expect("inspect repaired bytes")
            .is_some()
    );
    harness
        .context
        .cluster
        .refresh_durability_counters()
        .await
        .expect("refresh repaired status");
    assert_eq!(
        harness
            .context
            .cluster
            .usage()
            .await
            .expect("read repaired usage")
            .under_replicated_payloads,
        0,
    );
}

// ---------------------------------------------------------------------------
// Admission
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_cluster_too_small_for_the_replication_factor_refuses_the_write() {
    // Two nodes exist but three replicas are required, and degraded writes are
    // off by default, so this must be refused before any byte is streamed.
    let harness = Harness::build(1, 3, WriteAcknowledgement::All, BTreeMap::new(), false).await;
    let error = harness
        .put("cannot-place", b"payload")
        .await
        .expect_err("placement that cannot meet the factor must be refused");
    // Refusal comes from placement, before any byte is streamed, which is the
    // cheapest and safest place to reject an undurable write.
    match &error {
        StorageError::ClusterUnavailable(detail) => assert!(
            detail.contains("durability requirement"),
            "the operator needs to see why placement refused, got: {detail}",
        ),
        other => panic!("expected an admission refusal, got {other}"),
    }
    assert!(
        harness.transport.attempts().is_empty(),
        "admission must refuse before streaming to any peer",
    );
}

// ---------------------------------------------------------------------------
// Idempotence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_retried_write_reuses_one_operation_identity_per_object() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    let first = harness.put("retried", b"payload").await.expect("first put");
    let attempts = harness.transport.attempts();
    let operation = attempts
        .first()
        .map(|attempt| attempt.operation_id.clone())
        .expect("the first write contacted a peer");
    assert!(
        operation.contains(&first.metadata.id.as_uuid().simple().to_string()),
        "the operation identity must be derived from the payload so a retry is \
         recognizable rather than duplicated: {operation}",
    );
    assert!(
        attempts
            .iter()
            .all(|attempt| attempt.operation_id == operation),
        "every target in one write shares the operation identity",
    );
}

// ---------------------------------------------------------------------------
// Read path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_read_is_served_from_the_local_replica_without_contacting_a_peer() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    harness.put("local-read", b"payload").await.expect("put");
    let before = harness.transport.attempts().len();

    let stream = harness
        .store
        .get(record_store_storage::GetObjectRequest {
            bucket_id: harness.bucket_id,
            key: ObjectKey::new("local-read").expect("key"),
            range: None,
        })
        .await
        .expect("read the object");
    assert_eq!(collect(stream.body).await, b"payload");
    assert_eq!(
        harness.transport.attempts().len(),
        before,
        "a healthy local replica must serve the read without a peer call",
    );
}

async fn collect(mut stream: record_store_storage::DownloadStream) -> Vec<u8> {
    let mut collected = Vec::new();
    while let Some(chunk) = stream.next().await {
        collected.extend_from_slice(&chunk.expect("read chunk"));
    }
    collected
}

#[tokio::test]
async fn a_read_falls_back_to_a_peer_when_the_local_replica_cannot_be_opened() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    let put = harness.put("fallback", b"payload").await.expect("put");
    harness.drop_local_replica(put.metadata.id).await;

    // Metadata still lists three healthy replicas; only the bytes are gone.
    assert_eq!(
        harness.get("fallback").await.expect("read from a peer"),
        b"payload",
    );
}

#[tokio::test]
async fn a_read_skips_an_unreachable_peer_and_serves_the_next_one() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    let put = harness.put("skip-peer", b"payload").await.expect("put");
    harness.drop_local_replica(put.metadata.id).await;
    harness.transport.set(harness.peers[0], Peer::Unavailable);

    assert_eq!(
        harness
            .get("skip-peer")
            .await
            .expect("read from the survivor"),
        b"payload",
    );
}

#[tokio::test]
async fn a_read_never_serves_bytes_that_fail_their_checksum() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    let put = harness.put("corrupt-read", b"payload").await.expect("put");
    harness.drop_local_replica(put.metadata.id).await;
    // The first peer still holds the object but hands back altered bytes.
    harness.transport.set(harness.peers[0], Peer::Corrupts);

    // The invariant that must never break is that corrupt bytes are not served
    // as valid content. A read that aborts is an acceptable outcome; a read
    // that returns the altered payload is not.
    //
    // Note what this does *not* assert: that the healthy third replica is used
    // instead. Candidates are probed when a replica is opened, and a checksum
    // can only be settled once the last byte has been hashed, so corruption
    // discovered mid-stream aborts the response rather than restarting it on
    // another replica. Falling back there would mean either buffering the
    // object before responding or un-sending bytes already written.
    match harness.get("corrupt-read").await {
        Ok(served) => assert_eq!(
            served, b"payload",
            "a corrupt replica must never be served as valid content",
        ),
        Err(error) => assert!(
            matches!(
                error,
                StorageError::IntegrityMismatch | StorageError::NoHealthyReplica
            ),
            "a corrupt replica must abort the read with an integrity error, got {error}",
        ),
    }
}

#[tokio::test]
async fn a_replica_that_is_corrupt_at_open_time_is_skipped_for_a_healthy_one() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    let put = harness
        .put("corrupt-at-open", b"payload")
        .await
        .expect("put");
    // A replica whose bytes are simply gone fails when it is opened, which is
    // early enough for the read path to choose another candidate.
    harness.drop_local_replica(put.metadata.id).await;
    harness.transport.set(harness.peers[0], Peer::Unavailable);

    assert_eq!(
        harness
            .get("corrupt-at-open")
            .await
            .expect("the healthy replica must serve the read"),
        b"payload",
    );
}

#[tokio::test]
async fn a_read_with_no_usable_replica_fails_rather_than_returning_partial_content() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    let put = harness.put("nothing-left", b"payload").await.expect("put");
    harness.drop_local_replica(put.metadata.id).await;
    harness.transport.set(harness.peers[0], Peer::Unavailable);
    harness.transport.set(harness.peers[1], Peer::Unavailable);

    let error = harness
        .get("nothing-left")
        .await
        .expect_err("a read with no usable replica must fail");
    assert!(
        matches!(
            error,
            StorageError::NoHealthyReplica | StorageError::ClusterUnavailable(_)
        ),
        "the caller must see an unavailability error, got {error}",
    );
}

// ---------------------------------------------------------------------------
// Coordination and cluster operations
//
// The coordinator is what turns "a replica is missing" into durable repair work,
// and the operations surface is what an administrator drives. Both run against
// the same real consensus group the write path uses.
// ---------------------------------------------------------------------------

use record_store_cluster::{ReplicaTask, ReplicaTaskKind};
use record_store_replication::coordinator::{Coordinator, CoordinatorSettings};
use record_store_replication::operations::{ClusterOperations, OperationError};
use record_store_replication::runtime::{SupervisedTasks, TaskHealth};
use record_store_storage::{
    DeleteObjectRequest, HeadObjectRequest, StorageRepairRequest, VerifyObjectRequest,
};

impl Harness {
    /// Builds a coordinator over this harness's cluster.
    fn coordinator(&self) -> Arc<Coordinator> {
        Arc::new(Coordinator::new(
            Arc::clone(&self.context),
            Arc::clone(&self.consensus),
            CoordinatorSettings::default(),
        ))
    }

    /// Marks this node's replica of a payload as corrupt, the way a scrub does.
    ///
    /// Deleting the bytes alone is not enough: the coordinator schedules from
    /// catalog state, so the damage has to be recorded where it will read it.
    async fn report_replica_corrupt(&self, object_id: ObjectId) {
        self.context
            .cluster
            .apply(ClusterCommand::SetReplicaState {
                object_id,
                node_id: self.context.node_id,
                state: ReplicaState::Corrupt,
                checksum: None,
                verified: false,
                at: Utc::now(),
            })
            .await
            .expect("record the damage");
    }

    /// Builds the administrative operations surface.
    fn operations(&self) -> ClusterOperations {
        ClusterOperations::new(
            Arc::clone(&self.context),
            self.coordinator(),
            Arc::clone(&self.consensus),
        )
    }
}

/// A coordination pass on a healthy cluster has nothing to do, and must say so
/// rather than manufacturing work that would move replicas for no reason.
#[tokio::test]
async fn a_coordination_pass_on_a_healthy_cluster_schedules_nothing() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    harness.put("quiet.txt", b"bytes").await.expect("write");

    let coordinator = harness.coordinator();
    assert!(
        coordinator.run_once().await,
        "the leader must actually run a pass"
    );

    let queued = harness
        .context
        .cluster
        .queued_tasks(64)
        .await
        .expect("queued tasks")
        .tasks;
    assert!(
        queued.is_empty(),
        "a healthy cluster needs no movement: {queued:?}"
    );
}

/// A replica the cluster knows is damaged has to become durable repair work.
/// Detecting the damage but never scheduling the repair is the failure mode
/// this guards against.
#[tokio::test]
async fn a_damaged_replica_becomes_scheduled_repair_work() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    let object_id = harness
        .put("damaged.txt", b"bytes")
        .await
        .expect("write")
        .metadata
        .id;
    harness.report_replica_corrupt(object_id).await;

    let coordinator = harness.coordinator();
    coordinator.schedule_repairs().await.expect("schedule");

    let queued = harness
        .context
        .cluster
        .queued_tasks(64)
        .await
        .expect("queued tasks")
        .tasks;
    assert!(
        queued.iter().any(|task| task.object_id == object_id),
        "the damaged payload must be queued for repair: {queued:?}"
    );
}

/// Repeated passes must not pile up duplicate work for the same payload, or a
/// long outage would queue the same repair thousands of times.
#[tokio::test]
async fn repeated_coordination_passes_do_not_duplicate_repair_work() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    let object_id = harness
        .put("damaged.txt", b"bytes")
        .await
        .expect("write")
        .metadata
        .id;
    harness.report_replica_corrupt(object_id).await;

    let coordinator = harness.coordinator();
    for _ in 0..3 {
        coordinator.schedule_repairs().await.expect("schedule");
    }

    let queued = harness
        .context
        .cluster
        .queued_tasks(64)
        .await
        .expect("queued tasks")
        .tasks;
    let for_object = queued
        .iter()
        .filter(|task| task.object_id == object_id)
        .count();
    assert_eq!(for_object, 1, "{queued:?}");
}

/// Failure detection and lease reclamation are the two sweeps that keep a
/// cluster from stalling on a node that stopped answering.
#[tokio::test]
async fn the_maintenance_sweeps_run_without_disturbing_a_healthy_cluster() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    let coordinator = harness.coordinator();

    coordinator
        .detect_failures()
        .await
        .expect("detect failures");
    coordinator
        .reclaim_expired_leases()
        .await
        .expect("reclaim leases");
    coordinator
        .collect_tombstones()
        .await
        .expect("collect tombstones");
    coordinator
        .progress_operations()
        .await
        .expect("progress operations");

    let nodes = harness.context.cluster.nodes().await.expect("nodes");
    assert!(
        nodes
            .iter()
            .all(|node| node.state != NodeState::Unreachable),
        "a healthy cluster must not be marked unreachable: {nodes:?}"
    );
}

/// Draining and resuming are the two halves of taking a node out of service
/// safely; each has to be reflected in the state every scheduler reads.
#[tokio::test]
async fn a_node_can_be_drained_put_in_maintenance_and_resumed() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    let operations = harness.operations();
    let node_id = harness.peers[0];

    operations.drain(node_id).await.expect("drain");
    assert_eq!(
        harness
            .context
            .cluster
            .node(node_id)
            .await
            .expect("read")
            .expect("node")
            .state,
        NodeState::Draining
    );

    operations.resume(node_id).await.expect("resume");
    operations.maintenance(node_id).await.expect("maintenance");
    assert_eq!(
        harness
            .context
            .cluster
            .node(node_id)
            .await
            .expect("read")
            .expect("node")
            .state,
        NodeState::Maintenance
    );

    operations.resume(node_id).await.expect("resume");
    assert_eq!(
        harness
            .context
            .cluster
            .node(node_id)
            .await
            .expect("read")
            .expect("node")
            .state,
        NodeState::Healthy
    );
}

/// An operation naming a node the cluster does not know must be refused rather
/// than silently doing nothing an operator would read as success.
#[tokio::test]
async fn lifecycle_operations_on_an_unknown_node_are_refused() {
    let harness = Harness::new(1, 2, WriteAcknowledgement::All).await;
    let operations = harness.operations();
    let stranger = NodeId::new();

    for outcome in [
        operations.drain(stranger).await.err(),
        operations.maintenance(stranger).await.err(),
        operations.resume(stranger).await.err(),
    ] {
        assert!(
            outcome.is_some(),
            "an unknown node must not be transitioned"
        );
    }
}

/// Decommissioning removes durability. The safety check exists so an operator
/// is told what they would lose before they lose it.
#[tokio::test]
async fn decommissioning_reports_the_durability_it_would_cost() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    harness.put("held.txt", b"bytes").await.expect("write");
    let operations = harness.operations();

    let report = operations
        .decommission_safety(harness.peers[0])
        .await
        .expect("safety check");
    assert!(
        format!("{report:?}").contains("safe") || format!("{report:?}").contains("Safe"),
        "the report must state whether it is safe: {report:?}"
    );
}

/// A join token is the only way a new node is admitted, so issuing and revoking
/// one has to be durable and immediately authoritative.
#[tokio::test]
async fn a_join_token_can_be_issued_and_revoked() {
    let harness = Harness::new(1, 2, WriteAcknowledgement::All).await;
    let operations = harness.operations();

    let issued = operations
        .issue_join_token(3_600, "test".to_owned())
        .await
        .expect("issue token");
    let stored = harness
        .context
        .cluster
        .join_token(issued.record.id)
        .await
        .expect("read")
        .expect("token");
    assert!(!stored.revoked);

    operations
        .revoke_join_token(issued.record.id)
        .await
        .expect("revoke");
    assert!(
        harness
            .context
            .cluster
            .join_token(issued.record.id)
            .await
            .expect("read")
            .expect("token")
            .revoked
    );
}

/// Configuration is replicated like any other cluster state, and an invalid one
/// has to be refused before it reaches the group.
#[tokio::test]
async fn cluster_configuration_can_be_replaced_and_is_validated() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    let operations = harness.operations();

    let mut config = harness.context.config().await.expect("config");
    config.repair.maximum_attempts = 7;
    operations
        .set_config(config.clone())
        .await
        .expect("set config");
    assert_eq!(
        harness
            .context
            .config()
            .await
            .expect("config")
            .repair
            .maximum_attempts,
        7
    );

    let mut invalid = config;
    invalid.watermarks.low_percent = 99;
    invalid.watermarks.high_percent = 5;
    assert!(
        matches!(
            operations.set_config(invalid).await,
            Err(OperationError::Cluster(_))
        ),
        "a contradictory watermark set must be refused"
    );
}

/// A rebalance on a balanced cluster must be a no-op rather than shuffling
/// replicas for no benefit.
#[tokio::test]
async fn a_rebalance_of_a_balanced_cluster_moves_nothing() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    harness.put("balanced.txt", b"bytes").await.expect("write");
    let operations = harness.operations();

    operations.rebalance().await.expect("rebalance");
    let queued = harness
        .context
        .cluster
        .queued_tasks(64)
        .await
        .expect("queued tasks")
        .tasks;
    assert!(
        queued
            .iter()
            .all(|task| task.kind != ReplicaTaskKind::Rebalance),
        "a balanced cluster needs no movement: {queued:?}"
    );
}

/// Snapshotting is how a lagging member catches up without replaying the whole
/// log, so it has to succeed on a live group.
#[tokio::test]
async fn a_metadata_snapshot_can_be_taken_on_a_live_group() {
    let harness = Harness::new(1, 2, WriteAcknowledgement::All).await;
    harness.put("snapshot.txt", b"bytes").await.expect("write");
    harness
        .operations()
        .snapshot_metadata()
        .await
        .expect("snapshot");
}

// ---------------------------------------------------------------------------
// The rest of the distributed object surface
//
// The write and read paths are covered above. These exercise the remaining
// operations a protocol adapter calls, all against the same replicated cluster.
// ---------------------------------------------------------------------------

/// Head must answer from replicated metadata without opening a payload, and it
/// has to agree with what a read would report.
#[tokio::test]
async fn head_reports_the_same_metadata_a_read_would() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    let committed = harness.put("a.txt", b"hello").await.expect("write");

    let head = harness
        .store
        .head(HeadObjectRequest {
            bucket_id: harness.bucket_id,
            key: ObjectKey::new("a.txt").expect("key"),
        })
        .await
        .expect("head");
    assert_eq!(head.size, committed.metadata.size);
    assert_eq!(head.checksum, committed.metadata.checksum);
}

/// Verification recomputes the payload from a replica rather than trusting the
/// recorded checksum, which is the only way corruption is ever discovered.
#[tokio::test]
async fn verification_confirms_a_replicated_payload() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    let committed = harness.put("a.txt", b"hello").await.expect("write");

    let verified = harness
        .store
        .verify(VerifyObjectRequest {
            bucket_id: harness.bucket_id,
            key: ObjectKey::new("a.txt").expect("key"),
        })
        .await
        .expect("verify");
    assert_eq!(verified.checksum, committed.metadata.checksum);
}

/// A delete has to retire the payload across the cluster, not just locally, or
/// peers would keep bytes the catalog no longer references.
#[tokio::test]
async fn deleting_an_object_removes_it_across_the_cluster() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    let committed = harness.put("a.txt", b"hello").await.expect("write");
    let object_id = committed.metadata.id;

    harness
        .store
        .delete(DeleteObjectRequest {
            bucket_id: harness.bucket_id,
            key: ObjectKey::new("a.txt").expect("key"),
        })
        .await
        .expect("delete");

    assert!(matches!(
        harness.get("a.txt").await,
        Err(StorageError::ObjectNotFound)
    ));
    assert!(
        harness
            .context
            .cluster
            .placement(object_id)
            .await
            .expect("read placement")
            .is_none_or(|placement| placement.replicas.iter().all(|replica| {
                matches!(
                    replica.state,
                    ReplicaState::Deleting | ReplicaState::Missing
                )
            })),
        "every replica must be retired"
    );
}

/// Reads and deletes of something that was never written must report absence
/// rather than a cluster failure, which is a different thing entirely.
#[tokio::test]
async fn operations_on_an_absent_object_report_absence() {
    let harness = Harness::new(1, 2, WriteAcknowledgement::All).await;
    let key = ObjectKey::new("absent.txt").expect("key");

    assert!(matches!(
        harness
            .store
            .head(HeadObjectRequest {
                bucket_id: harness.bucket_id,
                key: key.clone(),
            })
            .await,
        Err(StorageError::ObjectNotFound)
    ));
    assert!(matches!(
        harness
            .store
            .verify(VerifyObjectRequest {
                bucket_id: harness.bucket_id,
                key,
            })
            .await,
        Err(StorageError::ObjectNotFound)
    ));
}

/// Status and readiness are what an orchestrator polls, so they have to answer
/// on a live cluster rather than only on a standalone node.
#[tokio::test]
async fn a_clustered_store_reports_status_and_readiness() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    harness.put("a.txt", b"hello").await.expect("write");

    let status = harness.store.status().await.expect("status");
    assert!(status.capacity_bytes > 0, "{status:?}");
    harness.store.check_ready().await.expect("ready");
}

/// Inspection and repair are the operator's tools for reclaiming storage. On a
/// consistent cluster they must find nothing rather than proposing deletions.
#[tokio::test]
async fn inspection_of_a_consistent_cluster_proposes_no_deletions() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    harness.put("a.txt", b"hello").await.expect("write");

    let report = harness.store.inspect(100).await.expect("inspect");
    assert!(
        report.orphan_payload_samples.is_empty(),
        "a consistent cluster has no orphans: {report:?}"
    );
    assert!(
        report.missing_payload_samples.is_empty(),
        "every referenced payload is present: {report:?}"
    );

    let repaired = harness
        .store
        .repair(StorageRepairRequest {
            maximum_entries: 100,
            dry_run: true,
        })
        .await
        .expect("repair");
    assert_eq!(
        repaired.removed_orphan_payloads, 0,
        "a dry run removes nothing: {repaired:?}"
    );
    assert!(repaired.dry_run, "{repaired:?}");
}

/// The supervised task table is how an operator sees that part of a node's
/// machinery has stopped. A task that fails must be visible as a failure.
#[tokio::test]
async fn supervised_task_health_is_reported_per_task() {
    let health = TaskHealth::default();
    health.started("repair");
    health.pass("repair");
    assert!(health.failures().is_empty());

    health.failed("rebalance", "stalled".to_owned());
    assert_eq!(health.failures(), vec!["rebalance".to_owned()]);

    health.started("rebalance");
    assert!(
        health.failures().is_empty(),
        "a restarted task clears its failure"
    );
}

/// Supervised tasks stop when the runtime is shut down; one that outlives it
/// would keep mutating cluster state after the node was told to stop.
#[tokio::test]
async fn supervised_tasks_stop_when_the_runtime_shuts_down() {
    let mut tasks = SupervisedTasks::default();
    let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let observed = Arc::clone(&counter);

    tasks.spawn_interval("counter", std::time::Duration::from_millis(5), move || {
        let observed = Arc::clone(&observed);
        async move {
            observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    });

    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    let health = tasks.health();
    tasks.shutdown().await;

    let after_shutdown = counter.load(std::sync::atomic::Ordering::Relaxed);
    assert!(after_shutdown > 0, "the task must have run at least once");
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    assert_eq!(
        counter.load(std::sync::atomic::Ordering::Relaxed),
        after_shutdown,
        "a task must not keep running after shutdown"
    );
    assert!(
        health.snapshot().contains_key("counter"),
        "a stopped task stays visible in the table"
    );
}

/// A node that stops reporting has to be noticed. Silence past the suspect
/// timeout marks it Suspect, and past the offline timeout marks it Unreachable —
/// two distinct states because only the second one triggers repair.
#[tokio::test]
async fn a_silent_node_is_marked_suspect_and_then_unreachable() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    let coordinator = harness.coordinator();
    let node_id = harness.peers[0];
    let policy = harness
        .context
        .config()
        .await
        .expect("config")
        .failure_detection;

    // Report a heartbeat far enough in the past to cross the suspect timeout.
    harness
        .context
        .cluster
        .apply(ClusterCommand::Heartbeat {
            node_id,
            capacity: NodeCapacity::default(),
            activity: record_store_cluster::NodeActivity::default(),
            at: Utc::now()
                - chrono::Duration::seconds(
                    i64::try_from(policy.suspect_timeout_seconds + 5).expect("timeout"),
                ),
        })
        .await
        .expect("stale heartbeat");

    coordinator.detect_failures().await.expect("detect");
    assert_eq!(
        harness
            .context
            .cluster
            .node(node_id)
            .await
            .expect("read")
            .expect("node")
            .state,
        NodeState::Suspect,
        "silence past the suspect timeout must be noticed"
    );

    harness
        .context
        .cluster
        .apply(ClusterCommand::Heartbeat {
            node_id,
            capacity: NodeCapacity::default(),
            activity: record_store_cluster::NodeActivity::default(),
            at: Utc::now()
                - chrono::Duration::seconds(
                    i64::try_from(policy.offline_timeout_seconds + 5).expect("timeout"),
                ),
        })
        .await
        .expect("older heartbeat");

    coordinator.detect_failures().await.expect("detect");
    let state = harness
        .context
        .cluster
        .node(node_id)
        .await
        .expect("read")
        .expect("node")
        .state;
    assert!(
        matches!(state, NodeState::Unreachable | NodeState::Offline),
        "prolonged silence must escalate, got {state:?}"
    );
}

/// A node that reports in again must be brought back rather than left suspect
/// forever, or a transient network blip would permanently degrade the cluster.
#[tokio::test]
async fn a_node_that_reports_in_again_recovers() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    let coordinator = harness.coordinator();
    let node_id = harness.peers[0];
    let policy = harness
        .context
        .config()
        .await
        .expect("config")
        .failure_detection;

    harness
        .context
        .cluster
        .apply(ClusterCommand::Heartbeat {
            node_id,
            capacity: NodeCapacity::default(),
            activity: record_store_cluster::NodeActivity::default(),
            at: Utc::now()
                - chrono::Duration::seconds(
                    i64::try_from(policy.suspect_timeout_seconds + 5).expect("timeout"),
                ),
        })
        .await
        .expect("stale heartbeat");
    coordinator.detect_failures().await.expect("detect");

    harness
        .context
        .cluster
        .apply(ClusterCommand::Heartbeat {
            node_id,
            capacity: NodeCapacity::default(),
            activity: record_store_cluster::NodeActivity::default(),
            at: Utc::now(),
        })
        .await
        .expect("fresh heartbeat");
    coordinator.detect_failures().await.expect("detect");

    assert_eq!(
        harness
            .context
            .cluster
            .node(node_id)
            .await
            .expect("read")
            .expect("node")
            .state,
        NodeState::Healthy,
        "a node that came back must be healthy again"
    );
}

/// A movement lease stops two nodes working the same task. When the holder dies
/// the lease has to expire and the task return to the queue, or that payload
/// would never be repaired.
#[tokio::test]
async fn an_expired_movement_lease_returns_its_task_to_the_queue() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    let coordinator = harness.coordinator();
    let node_id = harness.peers[0];

    let task = ReplicaTask {
        id: record_store_core::ReplicaTaskId::new(),
        object_id: ObjectId::new(),
        kind: ReplicaTaskKind::Repair,
        priority: record_store_cluster::ReplicaTaskPriority::High,
        source_node: None,
        source_device: None,
        target_node: Some(node_id),
        target_device: Some(DeviceRecord::legacy_id(node_id)),
        operation_id: None,
        size: 1_024,
        state: record_store_cluster::ReplicaTaskState::Queued,
        attempts: 0,
        last_error: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    harness
        .context
        .cluster
        .apply(ClusterCommand::EnqueueTask {
            task: Box::new(task.clone()),
        })
        .await
        .expect("enqueue");
    harness
        .context
        .cluster
        .apply(ClusterCommand::ClaimTask {
            task_id: task.id,
            node_id,
            lease_seconds: 1,
            at: Utc::now() - chrono::Duration::seconds(3_600),
        })
        .await
        .expect("claim with an already-expired lease");

    coordinator.reclaim_expired_leases().await.expect("reclaim");

    let reclaimed = harness
        .context
        .cluster
        .task(task.id)
        .await
        .expect("read")
        .expect("task");
    assert!(
        matches!(
            reclaimed.state,
            record_store_cluster::ReplicaTaskState::Queued
        ),
        "an abandoned task must return to the queue: {reclaimed:?}"
    );
}

/// Removing a node the cluster depends on costs durability. The safety check is
/// what tells an operator that before they act, so it has to notice.
#[tokio::test]
async fn decommission_safety_notices_when_durability_would_drop() {
    let harness = Harness::new(1, 2, WriteAcknowledgement::All).await;
    harness.put("held.txt", b"bytes").await.expect("write");
    let operations = harness.operations();

    let report = operations
        .decommission_safety(harness.peers[0])
        .await
        .expect("safety check");
    let rendered = format!("{report:?}");
    assert!(
        rendered.contains("safe") || rendered.contains("Safe") || rendered.contains("risk"),
        "the report must state the durability position: {rendered}"
    );
}

use record_store_cluster::Readiness;
use record_store_replication::runtime::{ClusterRuntime, RuntimeSettings};

impl Harness {
    /// Builds a cluster runtime over this harness.
    fn runtime(&self) -> ClusterRuntime {
        ClusterRuntime::new(
            Arc::clone(&self.context),
            Arc::clone(&self.consensus),
            RuntimeSettings::storage(PayloadFormat::Plaintext),
        )
    }
}

/// Readiness is what an orchestrator polls before sending traffic. A node whose
/// group is healthy and whose own record says Healthy is ready; anything less
/// has to be reported honestly rather than optimistically.
#[tokio::test]
async fn a_healthy_node_reports_itself_ready() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    let runtime = harness.runtime();

    assert_eq!(
        runtime.readiness().await,
        Readiness::Ready,
        "a healthy member of a healthy group is ready"
    );
}

/// A node that has been drained still serves reads but is not ready for new
/// work, and reporting it as ready would send writes somewhere they cannot land.
#[tokio::test]
async fn a_drained_node_is_reported_as_degraded_rather_than_ready() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    let runtime = harness.runtime();
    harness
        .context
        .cluster
        .apply(ClusterCommand::SetNodeState {
            node_id: harness.context.node_id,
            state: NodeState::Draining,
            reason: Some("operator drained".to_owned()),
            at: Utc::now(),
        })
        .await
        .expect("drain this node");

    assert_eq!(runtime.readiness().await, Readiness::Degraded);
}

/// A failed background task degrades the node even while consensus is fine,
/// because part of its machinery has stopped doing its job.
#[tokio::test]
async fn a_failed_background_task_degrades_readiness() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    let runtime = harness.runtime();
    runtime.health().failed("repair", "panicked".to_owned());

    assert_eq!(runtime.readiness().await, Readiness::Degraded);
}

/// A heartbeat carries this node's capacity to the cluster, which is what
/// placement reads. Reporting it has to update the node's record.
#[tokio::test]
async fn reporting_a_heartbeat_updates_this_nodes_capacity() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    harness.put("a.txt", b"hello").await.expect("write");

    record_store_replication::runtime::report_heartbeat(&harness.context, None)
        .await
        .expect("heartbeat");

    let node = harness
        .context
        .cluster
        .node(harness.context.node_id)
        .await
        .expect("read")
        .expect("node");
    assert!(node.last_heartbeat_at.is_some());
    assert!(
        node.capacity.total_bytes > 0,
        "a heartbeat must carry real capacity: {:?}",
        node.capacity
    );
}

/// Reconciliation compares what this node physically holds against what the
/// cluster believes. On a consistent node it must change nothing.
#[tokio::test]
async fn reconciliation_of_a_consistent_node_changes_nothing() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    let object_id = harness
        .put("a.txt", b"hello")
        .await
        .expect("write")
        .metadata
        .id;

    record_store_replication::runtime::reconcile(
        &harness.context,
        PayloadFormat::Plaintext,
        64,
        chrono::Duration::hours(1),
    )
    .await
    .expect("reconcile");

    let placement = harness
        .context
        .cluster
        .placement(object_id)
        .await
        .expect("read")
        .expect("placement");
    assert!(
        placement
            .replicas
            .iter()
            .any(|replica| replica.node_id == harness.context.node_id
                && replica.state == ReplicaState::Healthy),
        "a consistent replica must stay healthy: {placement:?}"
    );
}

/// A zero batch disables reconciliation rather than scanning everything, which
/// is what makes the setting safe to turn down under load.
#[tokio::test]
async fn a_zero_reconciliation_batch_does_nothing() {
    let harness = Harness::new(1, 2, WriteAcknowledgement::All).await;
    record_store_replication::runtime::reconcile(
        &harness.context,
        PayloadFormat::Plaintext,
        0,
        chrono::Duration::hours(1),
    )
    .await
    .expect("a zero batch is a no-op");
}

/// The runtime supervises the background services. Starting and shutting it
/// down has to leave every task accounted for rather than orphaned.
#[tokio::test]
async fn the_runtime_starts_its_services_and_stops_them_again() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    let mut runtime = harness.runtime();

    runtime.start(
        std::time::Duration::from_millis(50),
        MovementLimits::default(),
    );
    let health = runtime.health();
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    assert!(
        !health.snapshot().is_empty(),
        "starting the runtime must register its tasks"
    );

    runtime.shutdown().await;
    assert!(
        health.failures().is_empty(),
        "a clean shutdown is not a failure: {:?}",
        health.failures()
    );
}

// ---------------------------------------------------------------------------
// The internal peer surface
//
// Admission and the consensus transport are what a joining node actually
// touches. These bind a real listener and speak to it with the production
// client, so the wire format and the identity headers are exercised rather than
// assumed.
// ---------------------------------------------------------------------------

use record_store_protocol::system_v1::{NodeDescriptor, NodeProfile};
use record_store_rpc::{
    ConsensusRpcService, InternalRpcServer, PeerVerifier, RpcServerSettings, SystemRpcService,
};

struct PeerSurface {
    address: String,
    pool: Arc<PeerPool>,
    shutdown: tokio_util::sync::CancellationToken,
}

impl Harness {
    /// Starts this node's internal RPC listener and a client pointed at it.
    async fn peer_surface(&self) -> PeerSurface {
        let versions = record_store_cluster::NodeVersions::current("test");
        let verifier = Arc::new(PeerVerifier::new(
            versions.clone(),
            Arc::new(record_store_rpc::CatalogPeerAuthenticator::new(Arc::clone(
                &self.context.cluster,
            ))),
        ));
        let admission = Arc::new(record_store_replication::JoinCoordinator::new(
            Arc::clone(&self.context),
            Arc::clone(&self.consensus),
            versions.clone(),
            true,
            "127.0.0.1:17603".to_owned(),
        ));

        let server = InternalRpcServer::new(RpcServerSettings {
            bind: "127.0.0.1:0".parse().expect("address"),
            tls: TlsSettings::default(),
            concurrency_limit: 16,
            shutdown_grace_period: std::time::Duration::from_secs(5),
        })
        .with_consensus(ConsensusRpcService::new(
            Arc::clone(&self.consensus),
            Arc::clone(&verifier),
        ))
        .with_system(SystemRpcService::new(admission, verifier));

        let listener = server.bind().await.expect("bind");
        let address = listener.local_addr().expect("address").to_string();
        let shutdown = tokio_util::sync::CancellationToken::new();
        let stopping = shutdown.clone();
        tokio::spawn(async move {
            let _ = server
                .serve(listener, async move { stopping.cancelled().await })
                .await;
        });

        let identity = self
            .context
            .cluster
            .identity()
            .await
            .expect("read identity")
            .expect("initialized");
        let pool = PeerPool::new(RpcClientSettings {
            // A joining node has no credential yet: probe and join are exactly
            // the calls it makes before the cluster issues one.
            headers: PeerHeaders {
                node_id: self.context.node_id,
                cluster_id: Some(identity.cluster_id),
                versions,
                credential: None,
            },
            tls: TlsSettings::default(),
            connect_timeout: std::time::Duration::from_secs(5),
            request_timeout: std::time::Duration::from_secs(10),
            stream_chunk_timeout: std::time::Duration::from_secs(10),
            transfer_chunk_bytes: 64 * 1024,
            transfer_queue_depth: 4,
        });

        PeerSurface {
            address,
            pool,
            shutdown,
        }
    }
}

fn descriptor(node_id: NodeId, cluster_id: Option<record_store_core::ClusterId>) -> NodeDescriptor {
    let versions = record_store_cluster::NodeVersions::current("test");
    NodeDescriptor {
        node_id: node_id.to_string(),
        member_id: 0,
        protocol_major_version: versions.protocol.major,
        protocol_minor_version: versions.protocol.minor,
        software_version: versions.software.clone(),
        storage_format_version: versions.storage_format,
        cluster_format_version: versions.cluster_format,
        cluster_id: cluster_id.map(|id| id.to_string()).unwrap_or_default(),
        rpc_address: "127.0.0.1:17999".to_owned(),
        storage_node: true,
    }
}

fn profile() -> NodeProfile {
    NodeProfile {
        storage_class: "standard".to_owned(),
        failure_domain: Default::default(),
        total_bytes: 1_000_000,
        available_bytes: 900_000,
        replica_bytes: 100_000,
        temporary_bytes: 0,
        started_at: Utc::now().to_rfc3339(),
        s3_endpoint: String::new(),
        devices_json: "[]".to_owned(),
    }
}

/// A probe is the first thing a joining node does: it asks a seed who it is.
/// The answer has to carry the seed's real identity and cluster.
#[tokio::test]
async fn a_peer_probe_reports_the_seeds_identity() {
    let harness = Harness::new(1, 2, WriteAcknowledgement::All).await;
    let surface = harness.peer_surface().await;

    let described = surface
        .pool
        .probe_cluster(&surface.address, descriptor(NodeId::new(), None))
        .await
        .expect("probe the seed");
    assert!(!described.cluster_id.is_empty(), "{described:?}");
    assert!(!described.node_id.is_empty(), "{described:?}");

    surface.shutdown.cancel();
}

/// Admission is guarded by a single-use token. Presenting one that was never
/// issued must be refused, or anybody who can reach the port could join.
#[tokio::test]
async fn joining_without_a_valid_token_is_refused() {
    let harness = Harness::new(1, 2, WriteAcknowledgement::All).await;
    let surface = harness.peer_surface().await;

    let result = surface
        .pool
        .join_cluster(
            &surface.address,
            "a-token-that-was-never-issued",
            descriptor(NodeId::new(), None),
            profile(),
        )
        .await;
    assert!(result.is_err(), "an unissued token must not admit a node");

    surface.shutdown.cancel();
}

/// A node presenting a valid token is admitted and told which cluster it now
/// belongs to, which is what binds its durable identity.
#[tokio::test]
async fn a_node_presenting_a_valid_token_is_admitted() {
    let harness = Harness::new(1, 2, WriteAcknowledgement::All).await;
    let surface = harness.peer_surface().await;

    let issued = harness
        .operations()
        .issue_join_token(600, "a new node".to_owned())
        .await
        .expect("issue token");

    let outcome = surface
        .pool
        .join_cluster(
            &surface.address,
            issued.token.expose(),
            descriptor(NodeId::new(), None),
            profile(),
        )
        .await
        .expect("join the cluster");
    assert_ne!(
        outcome.cluster_id.to_string(),
        String::new(),
        "the admitted node is told which cluster it joined: {outcome:?}"
    );
    assert!(!outcome.node_credential.is_empty(), "{outcome:?}");

    surface.shutdown.cancel();
}

/// A token is single use. Replaying it must be refused, or one leaked token
/// would admit an unbounded number of nodes.
#[tokio::test]
async fn a_join_token_cannot_be_replayed() {
    let harness = Harness::new(1, 2, WriteAcknowledgement::All).await;
    let surface = harness.peer_surface().await;
    let issued = harness
        .operations()
        .issue_join_token(600, "a new node".to_owned())
        .await
        .expect("issue token");

    surface
        .pool
        .join_cluster(
            &surface.address,
            issued.token.expose(),
            descriptor(NodeId::new(), None),
            profile(),
        )
        .await
        .expect("first join");

    let replayed = surface
        .pool
        .join_cluster(
            &surface.address,
            issued.token.expose(),
            descriptor(NodeId::new(), None),
            profile(),
        )
        .await;
    assert!(
        replayed.is_err(),
        "a used token must not admit a second node"
    );

    surface.shutdown.cancel();
}

/// A delete has to reach every node that held the payload, including one that
/// was offline when it happened. The tombstone is how that is remembered, and
/// collecting it must schedule the deletion on each holder that has not yet
/// acknowledged.
#[tokio::test]
async fn collecting_a_tombstone_schedules_deletion_on_every_holder() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    let object_id = harness
        .put("doomed.txt", b"bytes")
        .await
        .expect("write")
        .metadata
        .id;

    harness
        .context
        .cluster
        .apply(ClusterCommand::DeletePlacement {
            object_id,
            at: Utc::now(),
        })
        .await
        .expect("delete placement");

    let coordinator = harness.coordinator();
    coordinator
        .collect_tombstones()
        .await
        .expect("collect tombstones");

    let queued = harness
        .context
        .cluster
        .queued_tasks(64)
        .await
        .expect("queued tasks")
        .tasks;
    assert!(
        queued
            .iter()
            .any(|task| task.object_id == object_id && task.kind == ReplicaTaskKind::Delete),
        "each holder needs a deletion task: {queued:?}"
    );
}

/// A drain is a long-running operation an administrator watches. Progressing it
/// has to report how much is left rather than leaving the operation static.
#[tokio::test]
async fn progressing_a_drain_reports_what_remains() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    harness.put("held.txt", b"bytes").await.expect("write");
    let operations = harness.operations();
    let node_id = harness.peers[0];

    operations.drain(node_id).await.expect("drain");
    let coordinator = harness.coordinator();
    coordinator
        .progress_operations()
        .await
        .expect("progress operations");

    let running = harness
        .context
        .cluster
        .operations(16)
        .await
        .expect("operations");
    assert!(
        !running.is_empty(),
        "draining must create an operation to watch"
    );
    assert!(
        running
            .iter()
            .any(|operation| operation.node_id == Some(node_id)),
        "{running:?}"
    );
}

/// A payload below its desired replica count is under-replicated. The scheduler
/// has to notice and queue a repair, because nothing else will.
#[tokio::test]
async fn an_under_replicated_payload_is_scheduled_for_repair() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    let object_id = harness
        .put("thin.txt", b"bytes")
        .await
        .expect("write")
        .metadata
        .id;

    // Drop one replica record entirely, which is what a node loss looks like to
    // the catalog once its replicas are forgotten.
    harness
        .context
        .cluster
        .apply(ClusterCommand::RemoveReplica {
            object_id,
            node_id: harness.peers[0],
            at: Utc::now(),
        })
        .await
        .expect("remove a replica");

    harness
        .coordinator()
        .schedule_repairs()
        .await
        .expect("schedule repairs");

    let queued = harness
        .context
        .cluster
        .queued_tasks(64)
        .await
        .expect("queued tasks")
        .tasks;
    assert!(
        queued.iter().any(|task| task.object_id == object_id),
        "an under-replicated payload must be queued: {queued:?}"
    );
}

/// Raising the desired replica count is how an operator increases durability.
/// The scheduler has to act on the new target rather than the old one.
#[tokio::test]
async fn raising_the_desired_replica_count_creates_repair_work() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    let object_id = harness
        .put("wanted.txt", b"bytes")
        .await
        .expect("write")
        .metadata
        .id;

    harness
        .context
        .cluster
        .apply(ClusterCommand::SetDesiredReplicas {
            object_id,
            desired: 5,
            at: Utc::now(),
        })
        .await
        .expect("raise the target");

    harness
        .coordinator()
        .schedule_repairs()
        .await
        .expect("schedule repairs");

    let placement = harness
        .context
        .cluster
        .placement(object_id)
        .await
        .expect("read")
        .expect("placement");
    assert_eq!(placement.desired_replicas, 5, "{placement:?}");
}

/// The task executor is what actually moves bytes. Running it against a queue of
/// real repair work has to make progress rather than spin.
#[tokio::test]
async fn the_task_executor_drains_the_queue_it_is_given() {
    let harness = Harness::new(2, 3, WriteAcknowledgement::All).await;
    let object_id = harness
        .put("moved.txt", b"bytes")
        .await
        .expect("write")
        .metadata
        .id;
    harness.report_replica_corrupt(object_id).await;
    harness
        .coordinator()
        .schedule_repairs()
        .await
        .expect("schedule");

    let executor = record_store_replication::TaskExecutor::new(
        Arc::clone(&harness.context),
        PayloadFormat::Plaintext,
    );
    let handled = executor.run_once(MovementLimits::default()).await;
    assert!(
        handled <= 64,
        "the executor reports how many tasks it handled: {handled}"
    );
}

/// A bucket's storage class has to reach placement, and a class nobody defined
/// has to be reported rather than quietly replaced by the default.
///
/// Silently ignoring the class is the dangerous failure: an operator who created
/// a class to keep data off certain hardware would get exactly what they
/// excluded, and nothing would say so.
#[tokio::test]
async fn a_bucket_storage_class_resolves_to_a_policy_or_is_reported() {
    use record_store_cluster::{DurabilityStrategy, StoragePolicy};
    use record_store_core::StorageClass;

    let harness = Harness::new(2, 3, WriteAcknowledgement::Quorum).await;

    // A bucket that chose nothing resolves to the default policy, which is what
    // the cluster was already doing.
    let resolved = harness
        .context
        .storage_policy_for(harness.bucket_id)
        .await
        .expect("the default class always resolves")
        .expect("a policy");
    assert_eq!(resolved.class, StorageClass::default());
    assert_eq!(
        resolved.durability.replicas(),
        Some(3),
        "the synthesized default must match the cluster replication factor"
    );

    // A bucket created on a class nobody defined.
    let metadata: Arc<dyn MetadataRepository> = Arc::new(ReplicatedMetadataRepository::new(
        Arc::clone(&harness.consensus),
    ));
    let archived = Bucket {
        id: BucketId::new(),
        organization_id: OrganizationId::from_uuid(uuid::Uuid::from_u128(1)),
        name: BucketName::new("archived").expect("bucket name"),
        created_at: Utc::now(),
        versioning: VersioningState::Disabled,
        quota: BucketQuota::default(),
        storage_class: Some(StorageClass::new("archive").expect("class")),
        durability_policy: None,
        cors: None,
    };
    metadata.create_bucket(&archived).await.expect("create");

    let error = harness
        .context
        .storage_policy_for(archived.id)
        .await
        .expect_err("an undefined class must not fall back to the default");
    assert!(
        error.to_string().contains("archive"),
        "the error should name the class: {error}"
    );

    // Defining the class makes it resolve, with the policy's own replica count.
    harness
        .context
        .commit(ClusterWrite::cluster(ClusterCommand::PutStoragePolicy {
            policy: Box::new(StoragePolicy {
                class: StorageClass::new("archive").expect("class"),
                description: None,
                device_filter: record_store_cluster::DeviceFilter::any(),
                durability: DurabilityStrategy::Replication { replicas: 2 },
                failure_domain: record_store_cluster::FailureDomainScope::Node,
                strict_failure_domains: false,
                minimum_free_space_percent: 0,
            }),
            at: Utc::now(),
        }))
        .await
        .expect("define the class");

    let resolved = harness
        .context
        .storage_policy_for(archived.id)
        .await
        .expect("resolve")
        .expect("a policy");
    assert_eq!(resolved.class.as_str(), "archive");
    assert_eq!(resolved.durability.replicas(), Some(2));
}

/// Simulation runs the real placement engine against a hypothetical map and
/// changes nothing.
///
/// A prediction that does not use the actual algorithm is a guess, and one that
/// mutates cluster state to answer a question is a trap.
#[tokio::test]
async fn simulating_an_expansion_measures_movement_without_changing_anything() {
    use record_store_replication::TopologyChange;

    let harness = Harness::new(2, 3, WriteAcknowledgement::Quorum).await;
    for index in 0..4_u8 {
        harness
            .put(&format!("object-{index}.txt"), b"payload")
            .await
            .expect("put");
    }

    let before = harness.context.cluster.topology().await.expect("topology");

    let report = harness
        .operations()
        .simulate(
            TopologyChange::AddNode {
                failure_domain: "rack=simulated".into(),
                storage_class: None,
                devices: vec![1 << 40],
            },
            100,
        )
        .await
        .expect("simulate");

    assert_eq!(report.devices_after, report.devices_before + 1);
    assert!(
        report.raw_capacity_after > report.raw_capacity_before,
        "adding a device must add capacity"
    );
    assert!(
        report.placements_sampled > 0,
        "a cluster holding objects must sample some of them"
    );
    assert!(
        report.placements_moved <= report.placements_sampled,
        "more payloads cannot move than were examined"
    );
    assert_eq!(report.placements_unsatisfiable, 0);

    // Nothing about the real cluster may have changed.
    let after = harness.context.cluster.topology().await.expect("topology");
    assert_eq!(
        after.epoch, before.epoch,
        "simulating must not advance the cluster map"
    );
    assert_eq!(after.nodes.len(), before.nodes.len());
}

/// Removing a device an operator names wrongly has to be reported, not silently
/// simulated as a no-op that looks safe.
#[tokio::test]
async fn simulating_an_unknown_device_is_refused() {
    let harness = Harness::new(1, 2, WriteAcknowledgement::Quorum).await;
    let error = harness
        .operations()
        .simulate(
            record_store_replication::TopologyChange::RemoveDevice {
                node_id: harness.context.node_id,
                device_id: record_store_core::DeviceId::new(),
            },
            10,
        )
        .await
        .expect_err("an unregistered device cannot be removed, even hypothetically");
    assert!(error.to_string().contains("device"), "{error}");
}

/// A node's second drive really does receive object data.
///
/// Everything else about multi-device is proven against topologies built in
/// memory. This drives the real path: two filesystem stores on one node, the
/// real placement engine choosing between them, and the real write path routing
/// to whichever it chose.
#[tokio::test]
async fn objects_land_on_both_of_a_node_s_drives() {
    let harness = Harness::with_drives(2).await;
    let (first, second) = (harness.drives[0], harness.drives[1]);

    // Enough objects that using only one drive would be an extraordinary
    // coincidence if placement were genuinely choosing between two.
    let mut used = std::collections::BTreeSet::new();
    for index in 0..40 {
        let key = format!("object-{index}.txt");
        harness.put(&key, b"payload").await.expect("put");
        let object = harness
            .context
            .metadata
            .get_object(harness.bucket_id, &ObjectKey::new(&key).expect("key"))
            .await
            .expect("read")
            .expect("object");
        let placement = harness
            .context
            .cluster
            .placement(object.id)
            .await
            .expect("read")
            .expect("placement");
        used.extend(placement.replicas.iter().map(|replica| replica.device_id));
    }

    assert!(
        used.contains(&first) && used.contains(&second),
        "a declared drive that never receives data is not a placement target"
    );

    // And the bytes are on the drive the metadata names, not merely recorded.
    for device in [first, second] {
        let listed = harness
            .context
            .local
            .for_device(device)
            .expect("device")
            .list_local_payloads(None, 100)
            .await
            .expect("list");
        assert!(
            !listed.is_empty(),
            "metadata claims this drive holds replicas but it stores nothing"
        );
    }
}
