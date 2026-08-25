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
use oes_cluster::{
    CapacityAwarePlacement, ClusterCommand, ClusterConfig, ClusterIdentity, FailureDomain,
    NodeCapacity, NodeRegistration, NodeState, NodeVersions, ReplicaState, ReplicaTaskState,
    StorageClass, WriteAcknowledgement,
};
use oes_consensus::{
    ClusterStore, ClusterWrite, ConsensusSettings, MetadataConsensus, ReplicatedClusterStore,
    ReplicatedMetadataRepository,
};
use oes_core::{
    Bucket, BucketId, BucketName, BucketQuota, Checksum, ClusterId, NodeId, ObjectId, ObjectKey,
    OrganizationId, PayloadFormat, VersioningState,
};
use oes_metadata::MetadataRepository;
use oes_replication::{
    ClusterContext, DistributedObjectStore, DistributedSettings, TaskExecutor,
    tasks::MovementLimits,
};
use oes_rpc::{
    ConsensusNetwork, PeerHeaders, PeerPool, RemoteReadStream, RemoteReplicaVerification,
    RemoteReplicaWrite, ReplicaTarget, ReplicaTransport, RpcClientError, RpcClientSettings,
    TlsSettings, TransferExpectation, TransferStream,
};
use oes_storage::{
    LocalFilesystemStore, ObjectStore, PutObjectRequest, ReplicaStore, StorageError, upload_stream,
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
                    cluster_format_version: oes_cluster::CLUSTER_FORMAT_VERSION,
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
        let local = Arc::new(
            LocalFilesystemStore::open(
                directory.path().join("data"),
                directory.path().join("tmp"),
                local_metadata,
            )
            .await
            .expect("local store"),
        );
        let metadata: Arc<dyn MetadataRepository> =
            Arc::new(ReplicatedMetadataRepository::new(Arc::clone(&consensus)));
        let bucket = Bucket {
            id: BucketId::new(),
            organization_id: OrganizationId::from_uuid(uuid::Uuid::from_u128(1)),
            name: BucketName::new("durability").expect("bucket name"),
            created_at: Utc::now(),
            versioning: VersioningState::Disabled,
            quota: BucketQuota::default(),
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
            local: local.clone() as Arc<dyn ReplicaStore>,
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
        }
    }

    async fn put(
        &self,
        key: &str,
        payload: &[u8],
    ) -> Result<oes_storage::PutObjectResult, StorageError> {
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
            .get(oes_storage::GetObjectRequest {
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
        local: target_store.clone() as Arc<dyn ReplicaStore>,
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
        .get(oes_storage::GetObjectRequest {
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

async fn collect(mut stream: oes_storage::DownloadStream) -> Vec<u8> {
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
