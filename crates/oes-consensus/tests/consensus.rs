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
use oes_cluster::{
    ClusterCommand, ClusterConfig, ClusterIdentity, ClusterOutcome, FailureDomain, NodeCapacity,
    NodeRegistration, NodeVersions, StorageClass,
};
use oes_consensus::{
    ClusterWrite, ClusterWriteResponse, ConsensusError, ConsensusSettings, LeaderForwarder,
    MemberId, MemberNode, MetadataConsensus, OesTypeConfig,
};
use oes_core::{
    Bucket, BucketName, BucketQuota, ClusterId, NodeId, OrganizationId, VersioningState,
};
use oes_metadata::{MetadataCommand, MetadataRepository};
use openraft::{
    RaftNetwork, RaftNetworkFactory,
    error::{RPCError, RaftError, Unreachable},
    network::RPCOption,
    raft::{
        AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest,
        InstallSnapshotResponse, VoteRequest, VoteResponse,
    },
};

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

impl RaftNetwork<OesTypeConfig> for Connection {
    async fn append_entries(
        &mut self,
        request: AppendEntriesRequest<OesTypeConfig>,
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
        request: InstallSnapshotRequest<OesTypeConfig>,
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

impl RaftNetworkFactory<OesTypeConfig> for Router {
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
        peer.write(command.clone()).await
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
        cluster_format_version: oes_cluster::CLUSTER_FORMAT_VERSION,
        created_at: Utc::now(),
    }
}

fn bucket(name: &str) -> Bucket {
    Bucket {
        id: oes_core::BucketId::new(),
        organization_id: OrganizationId::from_uuid(uuid::Uuid::from_u128(1)),
        name: BucketName::new(name).expect("bucket name"),
        created_at: Utc::now(),
        versioning: VersioningState::Disabled,
        quota: BucketQuota::default(),
        durability_policy: None,
    }
}

fn registration() -> NodeRegistration {
    NodeRegistration {
        node_id: NodeId::new(),
        versions: NodeVersions::current("test"),
        rpc_address: "10.0.0.1:7603".into(),
        s3_endpoint: None,
        storage_class: StorageClass::default(),
        failure_domain: FailureDomain::parse("rack=a").expect("labels"),
        capacity: NodeCapacity {
            total_bytes: 1_000,
            available_bytes: 900,
            replica_bytes: 100,
            temporary_bytes: 0,
        },
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
