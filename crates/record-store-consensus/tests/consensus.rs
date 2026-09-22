//! Consensus behaviour tests using an in-process transport.
#![expect(
    clippy::result_large_err,
    reason = "the consensus network trait fixes the error type"
)]

//!
//! The transport is deliberately simple so the tests exercise the real
//! consensus engine, the real durable log, and the real state machine, while
//! still being able to partition members and kill leaders deterministically.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use openraft::{
    RaftNetwork, RaftNetworkFactory,
    error::{RPCError, RaftError, Unreachable},
    network::RPCOption,
    raft::{
        AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest,
        InstallSnapshotResponse, VoteRequest, VoteResponse,
    },
};
use record_store_cluster::{
    ClusterCommand, ClusterConfig, ClusterHealth, ClusterIdentity, ClusterOutcome, FailureDomain,
    NodeCapacity, NodeRegistration, NodeVersions, StorageClass,
};
use record_store_consensus::{
    ClusterWrite, ClusterWriteResponse, ConsensusError, ConsensusSettings, LeaderForwarder,
    MemberId, MemberNode, MetadataConsensus, RecordStoreTypeConfig, RecoveryError, RecoveryIntent,
    SnapshotHealth,
};
use record_store_core::{
    Bucket, BucketName, BucketQuota, ClusterId, NodeId, OrganizationId, VersioningState,
};
use record_store_metadata::{MetadataCommand, MetadataRepository};

type Registry = Arc<Mutex<BTreeMap<MemberId, Arc<MetadataConsensus>>>>;
type Partitions = Arc<Mutex<BTreeSet<(MemberId, MemberId)>>>;

#[derive(Clone)]
struct Router {
    local: MemberId,
    members: Registry,
    partitions: Partitions,
}

impl Router {
    fn new(local: MemberId, members: Registry, partitions: Partitions) -> Self {
        Self {
            local,
            members,
            partitions,
        }
    }

    fn blocked(&self, target: MemberId) -> bool {
        let partitions = self.partitions.lock().expect("partition table");
        partitions.contains(&(self.local, target)) || partitions.contains(&(target, self.local))
    }

    fn peer(&self, target: MemberId) -> Option<Arc<MetadataConsensus>> {
        self.members
            .lock()
            .expect("member registry")
            .get(&target)
            .map(Arc::clone)
    }
}

struct Connection {
    router: Router,
    target: MemberId,
}

impl Connection {
    fn unreachable<E: std::error::Error + 'static>(
        &self,
        error: E,
    ) -> RPCError<MemberId, MemberNode, RaftError<MemberId>> {
        RPCError::Unreachable(Unreachable::new(&error))
    }

    fn resolve(
        &self,
    ) -> Result<Arc<MetadataConsensus>, RPCError<MemberId, MemberNode, RaftError<MemberId>>> {
        if self.router.blocked(self.target) {
            return Err(self.unreachable(std::io::Error::other("network partition")));
        }
        self.router
            .peer(self.target)
            .ok_or_else(|| self.unreachable(std::io::Error::other("member is not running")))
    }
}

impl RaftNetwork<RecordStoreTypeConfig> for Connection {
    async fn append_entries(
        &mut self,
        request: AppendEntriesRequest<RecordStoreTypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<MemberId>, RPCError<MemberId, MemberNode, RaftError<MemberId>>>
    {
        let peer = self.resolve()?;
        peer.handle_append_entries(request)
            .await
            .map_err(|error| self.unreachable(std::io::Error::other(error.to_string())))
    }

    async fn install_snapshot(
        &mut self,
        request: InstallSnapshotRequest<RecordStoreTypeConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<MemberId>,
        RPCError<MemberId, MemberNode, RaftError<MemberId, openraft::error::InstallSnapshotError>>,
    > {
        let peer = if self.router.blocked(self.target) {
            None
        } else {
            self.router.peer(self.target)
        };
        let Some(peer) = peer else {
            return Err(RPCError::Unreachable(Unreachable::new(
                &std::io::Error::other("member is not reachable"),
            )));
        };
        peer.handle_install_snapshot(request)
            .await
            .map_err(|error| {
                RPCError::Unreachable(Unreachable::new(&std::io::Error::other(error.to_string())))
            })
    }

    async fn vote(
        &mut self,
        request: VoteRequest<MemberId>,
        _option: RPCOption,
    ) -> Result<VoteResponse<MemberId>, RPCError<MemberId, MemberNode, RaftError<MemberId>>> {
        let peer = self.resolve()?;
        peer.handle_vote(request)
            .await
            .map_err(|error| self.unreachable(std::io::Error::other(error.to_string())))
    }
}

impl RaftNetworkFactory<RecordStoreTypeConfig> for Router {
    type Network = Connection;

    async fn new_client(&mut self, target: MemberId, _node: &MemberNode) -> Self::Network {
        Connection {
            router: self.clone(),
            target,
        }
    }
}

struct Forwarder {
    members: Registry,
    partitions: Partitions,
    local: MemberId,
}

#[async_trait]
impl LeaderForwarder for Forwarder {
    async fn forward_write(
        &self,
        leader: MemberId,
        _address: &str,
        command: &ClusterWrite,
    ) -> Result<ClusterWriteResponse, ConsensusError> {
        if self
            .partitions
            .lock()
            .expect("partition table")
            .contains(&(self.local, leader))
        {
            return Err(ConsensusError::Forward("network partition".into()));
        }
        let peer = self
            .members
            .lock()
            .expect("member registry")
            .get(&leader)
            .map(Arc::clone)
            .ok_or_else(|| ConsensusError::Forward("leader is not running".into()))?;
        // The production RPC handler proposes locally and refuses rather than
        // relaying onward, so the harness has to do the same or it would not be
        // modelling the wire behaviour it exists to test.
        peer.write_without_forwarding(command.clone()).await
    }

    async fn forward_read_barrier(
        &self,
        leader: MemberId,
        _address: &str,
    ) -> Result<Option<u64>, ConsensusError> {
        let peer = self
            .members
            .lock()
            .expect("member registry")
            .get(&leader)
            .map(Arc::clone)
            .ok_or_else(|| ConsensusError::Forward("leader is not running".into()))?;
        peer.read_barrier_index().await
    }
}

struct Harness {
    _directory: tempfile::TempDir,
    members: Registry,
    partitions: Partitions,
    directories: BTreeMap<MemberId, std::path::PathBuf>,
}

impl Harness {
    fn new() -> Self {
        Self {
            _directory: tempfile::tempdir().expect("temporary directory"),
            members: Arc::new(Mutex::new(BTreeMap::new())),
            partitions: Arc::new(Mutex::new(BTreeSet::new())),
            directories: BTreeMap::new(),
        }
    }

    fn directory(&mut self, member: MemberId) -> std::path::PathBuf {
        self.directories
            .entry(member)
            .or_insert_with(|| self._directory.path().join(format!("member-{member}")))
            .clone()
    }

    async fn start(&mut self, member: MemberId) -> Arc<MetadataConsensus> {
        let directory = self.directory(member);
        let mut settings =
            ConsensusSettings::new(member, format!("member-{member}:7603"), directory);
        settings.heartbeat_interval_millis = 60;
        settings.election_timeout_min_millis = 300;
        settings.election_timeout_max_millis = 600;
        settings.snapshot_logs_threshold = 32;
        settings.retained_logs = 8;
        settings.operation_timeout = Duration::from_secs(10);
        let router = Router::new(
            member,
            Arc::clone(&self.members),
            Arc::clone(&self.partitions),
        );
        let consensus = MetadataConsensus::start(settings, router)
            .await
            .expect("start consensus");
        consensus
            .set_leader_forwarder(Arc::new(Forwarder {
                members: Arc::clone(&self.members),
                partitions: Arc::clone(&self.partitions),
                local: member,
            }))
            .await;
        self.members
            .lock()
            .expect("member registry")
            .insert(member, Arc::clone(&consensus));
        consensus
    }

    async fn stop(&self, member: MemberId) {
        let removed = self
            .members
            .lock()
            .expect("member registry")
            .remove(&member);
        if let Some(consensus) = removed {
            consensus.shutdown().await;
        }
    }

    fn isolate(&self, member: MemberId, peers: &[MemberId]) {
        let mut partitions = self.partitions.lock().expect("partition table");
        for peer in peers {
            partitions.insert((member, *peer));
            partitions.insert((*peer, member));
        }
    }

    fn heal(&self) {
        self.partitions.lock().expect("partition table").clear();
    }

    async fn leader(&self, timeout: Duration) -> Arc<MetadataConsensus> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let candidates: Vec<_> = self
                .members
                .lock()
                .expect("member registry")
                .values()
                .map(Arc::clone)
                .collect();
            for candidate in candidates {
                if candidate.is_leader().await {
                    return candidate;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no leader was elected within {timeout:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn identity() -> ClusterIdentity {
    ClusterIdentity {
        cluster_id: ClusterId::new(),
        cluster_format_version: record_store_cluster::CLUSTER_FORMAT_VERSION,
        created_at: Utc::now(),
        recovery_generation: 0,
        recovery_id: None,
        recovered_at: None,
    }
}

fn bucket(name: &str) -> Bucket {
    Bucket {
        id: record_store_core::BucketId::new(),
        organization_id: OrganizationId::from_uuid(uuid::Uuid::from_u128(1)),
        name: BucketName::new(name).expect("bucket name"),
        created_at: Utc::now(),
        versioning: VersioningState::Disabled,
        quota: BucketQuota::default(),
        storage_class: None,
        durability_policy: None,
        object_lock: None,
        cors: None,
    }
}

fn registration() -> NodeRegistration {
    NodeRegistration {
        node_id: NodeId::new(),
        versions: NodeVersions::current("test"),
        rpc_address: "10.0.0.1:7603".into(),
        s3_endpoint: None,
        management_endpoint: None,
        storage_class: StorageClass::default(),
        failure_domain: FailureDomain::parse("rack=a").expect("labels"),
        capacity: NodeCapacity {
            total_bytes: 1_000,
            available_bytes: 900,
            replica_bytes: 100,
            temporary_bytes: 0,
        },
        devices: Vec::new(),
        started_at: Utc::now(),
    }
}

async fn bootstrap(harness: &mut Harness, members: &[MemberId]) -> Arc<MetadataConsensus> {
    let first = harness.start(members[0]).await;
    first
        .initialize_single_member()
        .await
        .expect("initialize consensus");
    first
        .wait_for_leader(Duration::from_secs(10))
        .await
        .expect("elect leader");
    first
        .write(ClusterWrite::cluster(ClusterCommand::InitializeCluster {
            identity: identity(),
            config: Box::new(ClusterConfig::default()),
        }))
        .await
        .expect("initialize cluster state");
    for member in &members[1..] {
        harness.start(*member).await;
        first
            .add_member(*member, format!("member-{member}:7603"), true)
            .await
            .expect("add member");
    }
    first
}

#[tokio::test]
async fn a_single_member_group_commits_and_survives_restart() {
    let mut harness = Harness::new();
    let leader = bootstrap(&mut harness, &[1]).await;
    let record = bucket("single-member");
    leader
        .write(ClusterWrite::metadata(MetadataCommand::CreateBucket {
            bucket: Box::new(record.clone()),
        }))
        .await
        .expect("create bucket")
        .into_metadata()
        .expect("bucket outcome");
    let quorum = leader.quorum().await;
    assert_eq!(quorum.status.members, 1);
    assert!(quorum.status.writable);
    assert!(
        !quorum.status.fault_tolerant,
        "a one-member metadata group must not claim fault tolerance"
    );

    drop(leader);
    harness.stop(1).await;
    // Give the shut-down consensus task time to release the durable files it
    // owns before the same data directory is reopened.
    tokio::time::sleep(Duration::from_millis(250)).await;
    let restarted = harness.start(1).await;
    restarted
        .wait_for_leader(Duration::from_secs(10))
        .await
        .expect("re-elect leader");
    let stored = restarted
        .state()
        .metadata()
        .get_bucket_by_name(&record.name)
        .await
        .expect("read bucket")
        .expect("bucket must survive a restart");
    assert_eq!(stored.id, record.id);
}

#[tokio::test]
async fn a_follower_does_not_report_unobserved_voters_as_unreachable() {
    let mut harness = Harness::new();
    let leader = bootstrap(&mut harness, &[1, 2, 3]).await;
    let follower = harness
        .members
        .lock()
        .expect("member registry")
        .values()
        .find(|candidate| !Arc::ptr_eq(candidate, &leader))
        .map(Arc::clone)
        .expect("a follower must exist");

    let quorum = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let quorum = follower.quorum().await;
            if quorum.status.members == 3 && quorum.status.leader.is_some() {
                break quorum;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the follower must learn the complete quorum");

    assert_eq!(quorum.role, "follower");
    assert_eq!(quorum.status.healthy_members, None);
    assert_eq!(quorum.status.health, ClusterHealth::Healthy);
    assert!(quorum.status.writable);
    assert_eq!(
        quorum
            .members
            .iter()
            .filter(|member| member.reachable.is_none())
            .count(),
        1,
        "the follower should leave the other non-leader voter's reachability unknown"
    );
}

/// The consistency boundary that cluster startup depends on.
///
/// A joining node's registration is committed by the leader, remotely. Reading
/// the local catalog straight afterwards races the replication and application
/// of that commit, which surfaced as a spurious `NodeNotRegistered` during
/// three-node startup. After a read barrier the registration must be visible in
/// the follower's own applied state on the *first* read.
#[tokio::test]
async fn a_read_barrier_makes_a_leader_commit_visible_to_a_follower_immediately() {
    let mut harness = Harness::new();
    let leader = bootstrap(&mut harness, &[1, 2, 3]).await;

    let node = registration();
    let node_id = node.node_id;
    leader
        .write(ClusterWrite::cluster(ClusterCommand::RegisterNode {
            registration: Box::new(node),
            at: Utc::now(),
        }))
        .await
        .expect("register the joining node");

    let follower = harness
        .members
        .lock()
        .expect("member registry")
        .get(&3)
        .map(Arc::clone)
        .expect("follower is running");
    follower
        .ensure_read_consistency()
        .await
        .expect("establish the read barrier");

    // Exactly one read. Polling here would hide the race this guards against:
    // the point is that the barrier alone is sufficient.
    let stored = follower
        .state()
        .cluster()
        .node(node_id)
        .await
        .expect("read the cluster catalog");
    assert!(
        stored.is_some(),
        "a read barrier must make the leader's commit visible to the follower's applied state",
    );
}

#[tokio::test]
async fn writes_replicate_to_every_member() {
    let mut harness = Harness::new();
    let leader = bootstrap(&mut harness, &[1, 2, 3]).await;
    let record = bucket("replicated");
    leader
        .write(ClusterWrite::metadata(MetadataCommand::CreateBucket {
            bucket: Box::new(record.clone()),
        }))
        .await
        .expect("create bucket");

    for member in [1_u64, 2, 3] {
        let peer = harness
            .members
            .lock()
            .expect("member registry")
            .get(&member)
            .map(Arc::clone)
            .expect("member is running");
        // Every member applies the same committed log, so each one must
        // eventually hold the identical record.
        let mut attempts = 0;
        loop {
            let stored = peer
                .state()
                .metadata()
                .get_bucket_by_name(&record.name)
                .await
                .expect("read bucket");
            if stored.is_some() {
                break;
            }
            attempts += 1;
            assert!(attempts < 100, "member {member} never applied the write");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    let quorum = leader.quorum().await;
    assert_eq!(quorum.status.members, 3);
    assert!(quorum.status.fault_tolerant);
}

#[tokio::test]
async fn followers_forward_writes_to_the_leader() {
    let mut harness = Harness::new();
    let leader = bootstrap(&mut harness, &[1, 2, 3]).await;
    let follower = harness
        .members
        .lock()
        .expect("member registry")
        .values()
        .find(|candidate| !Arc::ptr_eq(candidate, &leader))
        .map(Arc::clone)
        .expect("a follower must exist");
    let record = bucket("forwarded");
    follower
        .write(ClusterWrite::metadata(MetadataCommand::CreateBucket {
            bucket: Box::new(record.clone()),
        }))
        .await
        .expect("a follower must forward the write to the leader");
    follower
        .ensure_read_consistency()
        .await
        .expect("read barrier");
    assert!(
        follower
            .state()
            .metadata()
            .get_bucket_by_name(&record.name)
            .await
            .expect("read bucket")
            .is_some(),
        "a read after a successful write must observe it"
    );
}

#[tokio::test]
async fn a_new_leader_is_elected_when_the_leader_is_killed() {
    let mut harness = Harness::new();
    let leader = bootstrap(&mut harness, &[1, 2, 3]).await;
    let record = bucket("before-failover");
    leader
        .write(ClusterWrite::metadata(MetadataCommand::CreateBucket {
            bucket: Box::new(record.clone()),
        }))
        .await
        .expect("create bucket");
    let leader_id = leader.member_id();
    drop(leader);
    harness.stop(leader_id).await;

    let new_leader = harness.leader(Duration::from_secs(15)).await;
    assert_ne!(new_leader.member_id(), leader_id);
    let after = bucket("after-failover");
    new_leader
        .write(ClusterWrite::metadata(MetadataCommand::CreateBucket {
            bucket: Box::new(after.clone()),
        }))
        .await
        .expect("the new leader must accept writes");
    new_leader
        .ensure_read_consistency()
        .await
        .expect("read barrier");
    let metadata = new_leader.state();
    assert!(
        metadata
            .metadata()
            .get_bucket_by_name(&record.name)
            .await
            .expect("read")
            .is_some(),
        "committed metadata must survive a leader failure"
    );
    assert!(
        metadata
            .metadata()
            .get_bucket_by_name(&after.name)
            .await
            .expect("read")
            .is_some()
    );
}

#[tokio::test]
async fn a_minority_partition_cannot_accept_writes() {
    let mut harness = Harness::new();
    let leader = bootstrap(&mut harness, &[1, 2, 3]).await;
    let leader_id = leader.member_id();
    let others: Vec<MemberId> = [1_u64, 2, 3]
        .into_iter()
        .filter(|member| *member != leader_id)
        .collect();
    harness.isolate(leader_id, &others);

    let isolated = bucket("split-brain");
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        leader.write(ClusterWrite::metadata(MetadataCommand::CreateBucket {
            bucket: Box::new(isolated.clone()),
        })),
    )
    .await;
    assert!(
        !matches!(outcome, Ok(Ok(_))),
        "an isolated minority must not commit a metadata write"
    );

    // The majority elects a new leader and keeps serving writes.
    let majority = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            for member in &others {
                let peer = harness
                    .members
                    .lock()
                    .expect("member registry")
                    .get(member)
                    .map(Arc::clone);
                if let Some(peer) = peer
                    && peer.is_leader().await
                {
                    return peer;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the majority partition must elect a leader");

    let accepted = bucket("majority-write");
    majority
        .write(ClusterWrite::metadata(MetadataCommand::CreateBucket {
            bucket: Box::new(accepted.clone()),
        }))
        .await
        .expect("the majority must keep accepting writes");

    harness.heal();
    // After healing, the previously isolated member must converge on the
    // majority's state rather than keeping its own.
    let converged = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let stored = leader
                .state()
                .metadata()
                .get_bucket_by_name(&accepted.name)
                .await
                .expect("read");
            if stored.is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    assert!(
        converged.is_ok(),
        "the healed member must adopt the majority's committed state"
    );
    assert!(
        leader
            .state()
            .metadata()
            .get_bucket_by_name(&isolated.name)
            .await
            .expect("read")
            .is_none(),
        "an uncommitted minority write must never become visible"
    );
}

#[tokio::test]
async fn rejected_commands_do_not_stop_consensus() {
    let mut harness = Harness::new();
    let leader = bootstrap(&mut harness, &[1]).await;
    let record = bucket("duplicate");
    leader
        .write(ClusterWrite::metadata(MetadataCommand::CreateBucket {
            bucket: Box::new(record.clone()),
        }))
        .await
        .expect("create bucket");
    let response = leader
        .write(ClusterWrite::metadata(MetadataCommand::CreateBucket {
            bucket: Box::new(record.clone()),
        }))
        .await
        .expect("a rejected command is still a successful round trip");
    assert!(matches!(response, ClusterWriteResponse::Rejected(_)));

    // Consensus must keep working after a rejection.
    let next = bucket("after-rejection");
    leader
        .write(ClusterWrite::metadata(MetadataCommand::CreateBucket {
            bucket: Box::new(next.clone()),
        }))
        .await
        .expect("consensus must continue after a rejection");
    assert!(
        leader
            .state()
            .metadata()
            .get_bucket_by_name(&next.name)
            .await
            .expect("read")
            .is_some()
    );
}

#[tokio::test]
async fn a_batch_write_commits_atomically() {
    let mut harness = Harness::new();
    let leader = bootstrap(&mut harness, &[1]).await;
    let record = bucket("atomic");
    let response = leader
        .write(ClusterWrite::batch([
            ClusterWrite::metadata(MetadataCommand::CreateBucket {
                bucket: Box::new(record.clone()),
            }),
            // The second command is invalid, so the whole batch must be
            // rejected and the first command must leave no trace.
            ClusterWrite::metadata(MetadataCommand::CreateBucket {
                bucket: Box::new(record.clone()),
            }),
        ]))
        .await
        .expect("round trip");
    assert!(matches!(response, ClusterWriteResponse::Rejected(_)));
    assert!(
        leader
            .state()
            .metadata()
            .get_bucket_by_name(&record.name)
            .await
            .expect("read")
            .is_none(),
        "a rejected batch must not leave partial state behind"
    );
}

#[tokio::test]
async fn snapshots_compact_the_log_and_transfer_to_a_new_member() {
    let mut harness = Harness::new();
    let leader = bootstrap(&mut harness, &[1]).await;
    // The threshold is 32 entries, so this comfortably triggers snapshotting.
    for index in 0..60 {
        leader
            .write(ClusterWrite::cluster(ClusterCommand::RegisterNode {
                registration: Box::new(registration()),
                at: Utc::now(),
            }))
            .await
            .unwrap_or_else(|error| panic!("register node {index}: {error}"));
    }
    leader.trigger_snapshot().await.expect("trigger snapshot");
    let snapshotted = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if leader.quorum().await.snapshot_index.is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(snapshotted.is_ok(), "a snapshot must be built");

    harness.start(2).await;
    leader
        .add_member(2, "member-2:7603".into(), true)
        .await
        .expect("add member");
    let peer = harness
        .members
        .lock()
        .expect("member registry")
        .get(&2)
        .map(Arc::clone)
        .expect("member is running");
    let caught_up = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let nodes = peer.state().cluster().nodes().await.expect("read nodes");
            if nodes.len() == 60 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    assert!(
        caught_up.is_ok(),
        "a new member must receive the full state, including through a snapshot"
    );
}

#[tokio::test]
async fn cluster_and_object_metadata_commit_together() {
    let mut harness = Harness::new();
    let leader = bootstrap(&mut harness, &[1]).await;
    let outcome = leader
        .write(ClusterWrite::cluster(ClusterCommand::RegisterNode {
            registration: Box::new(registration()),
            at: Utc::now(),
        }))
        .await
        .expect("register node")
        .into_cluster()
        .expect("cluster outcome");
    let ClusterOutcome::Registration { raft_id, .. } = outcome else {
        panic!("registration must return a member identifier");
    };
    assert_eq!(raft_id, 1);
    let usage = leader.state().cluster().usage().await.expect("usage");
    assert_eq!(usage.payloads, 0);
}

/// A leader-elected scheduler must lose its authority the moment it loses
/// leadership, not merely fail to notice.
///
/// An ordinary write forwards: the client only wants the write to land
/// somewhere. A coordination write must not, because the thing that changed is
/// exactly this node's right to decide. Forwarding one would let a deposed
/// coordinator keep committing decisions through the node that replaced it, and
/// the cluster would be scheduled twice from two views of the world.
#[tokio::test]
async fn a_follower_cannot_commit_a_leader_fenced_write_by_forwarding_it() {
    let mut harness = Harness::new();
    let leader = bootstrap(&mut harness, &[1, 2, 3]).await;
    let term = leader
        .leadership_term()
        .await
        .expect("the bootstrapped member leads");

    let follower = harness
        .members
        .lock()
        .expect("member registry")
        .get(&3)
        .map(Arc::clone)
        .expect("follower is running");
    assert_eq!(
        follower.leadership_term().await,
        None,
        "a follower holds no leadership term"
    );

    // The same write that an ordinary client path would happily forward.
    let forwarded = follower.write(ClusterWrite::Noop).await;
    assert!(
        forwarded.is_ok(),
        "an ordinary write is expected to forward: {forwarded:?}"
    );

    let fenced = follower.write_as_leader(term, ClusterWrite::Noop).await;
    assert!(
        matches!(fenced, Err(ConsensusError::NoLeader)),
        "a fenced write from a non-leader must be refused, not forwarded: {fenced:?}"
    );
}

/// The term is what makes the fence a fence. A node that still leads, but in a
/// later term than the pass began in, has already been through an election; the
/// work that pass was doing was planned against a world that no longer holds.
#[tokio::test]
async fn a_write_fenced_to_a_stale_term_is_refused_by_the_current_leader() {
    let mut harness = Harness::new();
    let leader = bootstrap(&mut harness, &[1, 2, 3]).await;
    let term = leader
        .leadership_term()
        .await
        .expect("the bootstrapped member leads");

    leader
        .write_as_leader(term, ClusterWrite::Noop)
        .await
        .expect("the current term commits");

    let stale = leader.write_as_leader(term - 1, ClusterWrite::Noop).await;
    assert!(
        matches!(stale, Err(ConsensusError::NoLeader)),
        "a write fenced to a superseded term must be refused: {stale:?}"
    );
}

/// A redirect must cost one hop, not a chain.
///
/// The node receiving a forwarded write was told "you are the leader". If that
/// is no longer true, relaying the write onward is how a redirect becomes a
/// cycle: A forwards to B, B to C, C back to A, each hop spending another
/// request timeout and multiplying load precisely during the leader churn that
/// caused it. The receiver must refuse and name the leader it knows instead.
#[tokio::test]
async fn a_forwarded_write_is_never_forwarded_a_second_time() {
    let mut harness = Harness::new();
    let leader = bootstrap(&mut harness, &[1, 2, 3]).await;
    assert!(
        leader.leadership_term().await.is_some(),
        "member 1 must lead for this to test anything"
    );

    let first_follower = harness
        .members
        .lock()
        .expect("member registry")
        .get(&2)
        .map(Arc::clone)
        .expect("follower is running");
    let second_follower = harness
        .members
        .lock()
        .expect("member registry")
        .get(&3)
        .map(Arc::clone)
        .expect("follower is running");

    // What one follower would receive if another had forwarded to it by
    // mistake, or because leadership moved between the lookup and the call.
    let relayed = second_follower
        .write_without_forwarding(ClusterWrite::Noop)
        .await;
    let Err(error) = relayed else {
        panic!("a follower must not commit a write that was forwarded to it");
    };
    match error {
        ConsensusError::NotLeader { leader, .. } => {
            assert_eq!(leader, 1, "the refusal must name the leader it knows");
        }
        ConsensusError::NoLeader => {}
        other => panic!("expected a redirect, got {other:?}"),
    }

    // And the ordinary client path still works, in exactly one hop.
    first_follower
        .write(ClusterWrite::Noop)
        .await
        .expect("an ordinary write still reaches the leader");
}

// ---------------------------------------------------------------------------
// Disaster recovery
//
// These run against a real three-member group that is then reduced to one
// survivor. Nothing is mocked: the log, the state machine, and the snapshots are
// the same durable files a deployed member writes, and recovery is applied to
// them offline exactly as an operator would.

/// Builds a three-member cluster holding real object metadata, then keeps only
/// member 1's directory — the survivor of a two-of-three loss.
async fn cluster_reduced_to_one_survivor(
    harness: &mut Harness,
) -> (record_store_core::ClusterId, std::path::PathBuf) {
    let leader = bootstrap(harness, &[1, 2, 3]).await;
    let bucket_record = bucket("survivor");
    leader
        .write(ClusterWrite::metadata(MetadataCommand::CreateBucket {
            bucket: Box::new(bucket_record.clone()),
        }))
        .await
        .expect("create bucket");
    let cluster_id = leader
        .state()
        .cluster()
        .identity()
        .await
        .expect("read identity")
        .expect("initialized")
        .cluster_id;

    // The quorum is gone: two of three members are permanently lost. Every
    // handle has to go with them — recovery runs against a stopped node, and
    // redb holds the data files exclusively for as long as one is alive.
    drop(leader);
    harness.stop(2).await;
    harness.stop(3).await;
    harness.stop(1).await;
    (cluster_id, harness.directory(1))
}

/// Inspection is the step an operator runs on every survivor before choosing
/// which one to rebuild from. It must change nothing and must report enough to
/// make that choice — above all the applied position, which is what decides
/// which survivor is furthest ahead.
#[tokio::test]
async fn inspecting_a_survivor_reports_its_position_without_changing_it() {
    let mut harness = Harness::new();
    let (cluster_id, directory) = cluster_reduced_to_one_survivor(&mut harness).await;

    let first = record_store_consensus::recovery::inspect(&directory)
        .await
        .expect("inspect the survivor");
    assert!(first.recoverable, "{first:?}");
    assert_eq!(
        first.cluster.as_ref().map(|identity| identity.cluster_id),
        Some(cluster_id)
    );
    assert!(
        first.last_applied.is_some(),
        "the applied position is what an operator chooses a survivor by: {first:?}"
    );
    assert_eq!(
        first.voters.len(),
        3,
        "the survivor still records the group it belonged to: {first:?}"
    );

    let second = record_store_consensus::recovery::inspect(&directory)
        .await
        .expect("inspect again");
    assert_eq!(
        first, second,
        "inspection must not change what it reports on"
    );
}

/// The procedure itself: one survivor, offline, rebuilt into a cluster that
/// elects and serves — carrying its object history, its identity, and a record
/// that authority was rebuilt.
#[tokio::test]
async fn recovering_a_survivor_restores_a_working_cluster_with_its_history() {
    let mut harness = Harness::new();
    let (cluster_id, directory) = cluster_reduced_to_one_survivor(&mut harness).await;

    let report = record_store_consensus::recovery::recover_single_member(
        &directory,
        RecoveryIntent {
            cluster_id,
            member_id: 1,
            address: "member-1:7603".to_owned(),
            reason: "two of three voters were lost".to_owned(),
            accept_data_loss: true,
        },
    )
    .await
    .expect("recover the survivor");

    assert_eq!(report.cluster_id, cluster_id);
    assert_eq!(report.member_id, 1);
    assert_eq!(
        report.recovery_generation, 1,
        "the first rebuild of this cluster's authority"
    );
    assert_eq!(
        report.removed_voters,
        vec![2, 3],
        "the lost voters must be named, not silently dropped: {report:?}"
    );

    // The recovered member starts, elects alone, and still holds its history.
    let recovered = harness.start(1).await;
    recovered
        .wait_for_leader(Duration::from_secs(10))
        .await
        .expect("the recovered member must be able to elect");
    recovered
        .ensure_read_consistency()
        .await
        .expect("the recovered cluster must be readable");

    let buckets = recovered
        .state()
        .metadata()
        .list_buckets()
        .await
        .expect("read the restored catalog");
    assert!(
        buckets
            .iter()
            .any(|bucket| bucket.name.as_str() == "survivor"),
        "recovery rebuilds authority, it does not discard object history: {buckets:?}"
    );

    let identity = recovered
        .state()
        .cluster()
        .identity()
        .await
        .expect("read identity")
        .expect("initialized");
    assert_eq!(
        identity.cluster_id, cluster_id,
        "a recovered cluster keeps its identity rather than becoming a new one"
    );
    assert_eq!(identity.recovery_generation, 1);
    assert_eq!(identity.recovery_id, Some(report.recovery_id));

    // And it accepts new writes, which is the whole point of recovering.
    recovered
        .write(ClusterWrite::metadata(MetadataCommand::CreateBucket {
            bucket: Box::new(bucket("after-recovery")),
        }))
        .await
        .expect("the recovered cluster must accept writes");
}

/// The mistake that cannot be undone is recovering two survivors separately:
/// that leaves two clusters wearing one identifier. A single node cannot
/// prevent it, so each recovery stamps a distinct lineage, and the two are then
/// distinguishable rather than silently interchangeable.
#[tokio::test]
async fn two_independent_recoveries_of_one_cluster_are_distinguishable() {
    let mut harness = Harness::new();
    let leader = bootstrap(&mut harness, &[1, 2, 3]).await;
    let cluster_id = leader
        .state()
        .cluster()
        .identity()
        .await
        .expect("read identity")
        .expect("initialized")
        .cluster_id;
    // Both followers must have applied the cluster's initialization before the
    // group is torn down, or neither is a recoverable survivor.
    for member in [2, 3] {
        let follower = harness
            .members
            .lock()
            .expect("member registry")
            .get(&member)
            .map(Arc::clone)
            .expect("member is running");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while follower
            .state()
            .cluster()
            .identity()
            .await
            .expect("read identity")
            .is_none()
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "member {member} never replicated the cluster identity"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    drop(leader);
    for member in [1, 2, 3] {
        harness.stop(member).await;
    }

    let mut lineages = Vec::new();
    for member in [2, 3] {
        let report = record_store_consensus::recovery::recover_single_member(
            harness.directory(member),
            RecoveryIntent {
                cluster_id,
                member_id: member,
                address: format!("member-{member}:7603"),
                reason: "an operator recovered this survivor".to_owned(),
                accept_data_loss: true,
            },
        )
        .await
        .expect("recover");
        lineages.push(report.recovery_id);
    }

    assert_ne!(
        lineages[0], lineages[1],
        "two independent recoveries must not share a lineage, or the split they created \
         would be undetectable"
    );

    let first = record_store_consensus::recovery::inspect(harness.directory(2))
        .await
        .expect("inspect")
        .cluster
        .expect("identity");
    let second = record_store_consensus::recovery::inspect(harness.directory(3))
        .await
        .expect("inspect")
        .cluster
        .expect("identity");
    assert_eq!(
        first.cluster_id, second.cluster_id,
        "they are still the same cluster by name, which is exactly the trap"
    );
    assert!(
        !first.same_lineage(&second),
        "and they must not be treated as the same cluster: {first:?} vs {second:?}"
    );
}

/// Recovery is refused, specifically, whenever it would invent authority rather
/// than rebuild it. Each of these is a different operator mistake, and each has
/// to fail with a reason that names the mistake.
#[tokio::test]
async fn unsafe_recovery_attempts_are_refused_with_a_specific_reason() {
    let mut harness = Harness::new();
    let (cluster_id, directory) = cluster_reduced_to_one_survivor(&mut harness).await;

    let intent = |mutate: fn(&mut RecoveryIntent)| {
        let mut intent = RecoveryIntent {
            cluster_id,
            member_id: 1,
            address: "member-1:7603".to_owned(),
            reason: "test".to_owned(),
            accept_data_loss: true,
        };
        mutate(&mut intent);
        intent
    };

    // Not acknowledging the loss.
    let refused = record_store_consensus::recovery::recover_single_member(
        &directory,
        intent(|intent| intent.accept_data_loss = false),
    )
    .await;
    assert!(
        matches!(refused, Err(RecoveryError::LossNotAcknowledged)),
        "{refused:?}"
    );

    // Naming a different cluster, which is how a wrong data directory shows up.
    let refused = record_store_consensus::recovery::recover_single_member(
        &directory,
        intent(|intent| intent.cluster_id = record_store_core::ClusterId::new()),
    )
    .await;
    assert!(
        matches!(refused, Err(RecoveryError::ClusterMismatch { .. })),
        "{refused:?}"
    );

    // Recovering as a member the group never had.
    let refused = record_store_consensus::recovery::recover_single_member(
        &directory,
        intent(|intent| intent.member_id = 99),
    )
    .await;
    assert!(
        matches!(refused, Err(RecoveryError::NotAMember { .. })),
        "{refused:?}"
    );

    // A directory that holds no consensus state at all.
    let empty = tempfile::tempdir().expect("temporary directory");
    let refused =
        record_store_consensus::recovery::recover_single_member(empty.path(), intent(|_| {})).await;
    assert!(
        matches!(refused, Err(RecoveryError::NoConsensusState(_))),
        "{refused:?}"
    );

    // Every refusal must have left the survivor exactly as it was, or a failed
    // attempt would make the next one worse.
    let assessment = record_store_consensus::recovery::inspect(&directory)
        .await
        .expect("inspect");
    assert_eq!(assessment.voters.len(), 3, "{assessment:?}");
    assert_eq!(
        assessment
            .cluster
            .as_ref()
            .map(|identity| identity.recovery_generation),
        Some(0),
        "a refused recovery must not have advanced the lineage"
    );
}

/// A member that has applied nothing holds no authority to rebuild. Recovering
/// from it would produce an empty cluster wearing the old cluster's name, which
/// is worse than failing: it looks like it worked.
#[tokio::test]
async fn a_member_that_applied_nothing_cannot_be_recovered_from() {
    let mut harness = Harness::new();
    harness.start(9).await;
    harness.stop(9).await;

    let refused = record_store_consensus::recovery::recover_single_member(
        harness.directory(9),
        RecoveryIntent {
            cluster_id: record_store_core::ClusterId::new(),
            member_id: 9,
            address: "member-9:7603".to_owned(),
            reason: "test".to_owned(),
            accept_data_loss: true,
        },
    )
    .await;
    assert!(
        matches!(refused, Err(RecoveryError::NothingApplied)),
        "{refused:?}"
    );
}

/// A snapshot transfer or publication that was interrupted leaves a pointer
/// naming a file that is not there. The member still starts — the state machine,
/// not the snapshot, is what it restarts from — but an operator diagnosing the
/// failure has to be told, rather than shown a clean bill of health.
#[tokio::test]
async fn an_interrupted_snapshot_is_reported_rather_than_hidden() {
    let mut harness = Harness::new();
    let (_cluster_id, directory) = cluster_reduced_to_one_survivor(&mut harness).await;
    let snapshots = directory.join("snapshots");
    std::fs::create_dir_all(&snapshots).expect("snapshot directory");
    std::fs::write(
        snapshots.join("current.json"),
        br#"{"snapshot_id":"interrupted-1","last_applied":null,"last_membership":null}"#,
    )
    .expect("write a pointer to a snapshot that never landed");

    let assessment = record_store_consensus::recovery::inspect(&directory)
        .await
        .expect("inspect");
    let SnapshotHealth::Damaged { reason } = &assessment.snapshot else {
        panic!("an interrupted snapshot must be reported: {assessment:?}");
    };
    assert!(
        reason.contains("interrupted-1"),
        "the reason must name what is missing: {reason}"
    );

    // And the member still starts, because the snapshot is not what it restarts
    // from. A damaged snapshot is a diagnostic, not a wedge.
    let restarted = harness.start(1).await;
    restarted
        .state()
        .metadata()
        .list_buckets()
        .await
        .expect("the member still reads its own applied state");
}
