//! The Record Store-owned consensus boundary.
//!
//! Everything outside this crate talks to [`MetadataConsensus`]; no other crate
//! names a consensus library type. Leader redirection, read barriers, quorum
//! reporting, and snapshot policy all live behind this façade.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use openraft::{
    Config, Raft, RaftMetrics, RaftNetworkFactory, ServerState,
    error::{
        CheckIsLeaderError, ClientWriteError, Fatal, ForwardToLeader, InitializeError, RaftError,
    },
    raft::{
        AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest,
        InstallSnapshotResponse, VoteRequest, VoteResponse,
    },
};
use record_store_cluster::{ClusterHealth, QuorumStatus};
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::{
    command::{ClusterWrite, ClusterWriteResponse, CommandRejection, RejectionKind},
    log_store::{LogStoreError, RedbLogStore},
    state_machine::{ReplicatedState, StateMachineError, StateMachineStore},
    types::{MemberId, MemberNode, RecordStoreTypeConfig},
};

/// Failures raised by the consensus boundary.
#[derive(Debug, Error)]
pub enum ConsensusError {
    /// The durable consensus log could not be opened.
    #[error(transparent)]
    LogStore(#[from] LogStoreError),
    /// The replicated state could not be opened.
    #[error(transparent)]
    State(#[from] StateMachineError),
    /// Consensus configuration was invalid.
    #[error("invalid consensus configuration: {0}")]
    Configuration(String),
    /// The local member is not the leader and no leader is currently known.
    #[error(
        "cluster metadata has no leader right now; the operation cannot be completed until a \
         quorum elects one"
    )]
    NoLeader,
    /// The local member is not the leader and forwarding is not configured.
    #[error(
        "cluster metadata leader is member {leader} at {address}, and this node cannot forward"
    )]
    NotLeader {
        /// Member identifier of the current leader.
        leader: MemberId,
        /// Advertised address of the current leader.
        address: String,
    },
    /// A quorum could not be reached.
    #[error("cluster metadata quorum is unavailable: {0}")]
    QuorumUnavailable(String),
    /// The forwarded request failed.
    #[error("forwarding the metadata operation to the leader failed: {0}")]
    Forward(String),
    /// The consensus engine stopped.
    #[error("cluster metadata consensus stopped: {0}")]
    Stopped(String),
    /// The command was rejected by application rules.
    #[error(transparent)]
    Rejected(#[from] CommandRejection),
    /// An unexpected internal condition occurred.
    #[error("cluster metadata consensus failed: {0}")]
    Internal(String),
}

impl ConsensusError {
    /// Returns whether the caller may safely retry after a short delay.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        matches!(
            self,
            Self::NoLeader | Self::QuorumUnavailable(_) | Self::Forward(_)
        )
    }
}

/// Forwards metadata operations to the current leader.
///
/// Implemented by the internal RPC layer. Keeping it behind a trait means the
/// consensus crate never depends on a transport, and the S3 layer never learns
/// that leadership exists.
#[async_trait]
pub trait LeaderForwarder: Send + Sync {
    /// Sends a write to the leader and returns its replicated response.
    async fn forward_write(
        &self,
        leader: MemberId,
        address: &str,
        command: &ClusterWrite,
    ) -> Result<ClusterWriteResponse, ConsensusError>;

    /// Asks the leader for the log position a linearizable read must observe.
    async fn forward_read_barrier(
        &self,
        leader: MemberId,
        address: &str,
    ) -> Result<Option<u64>, ConsensusError>;
}

/// Node-local consensus settings.
#[derive(Debug, Clone)]
pub struct ConsensusSettings {
    /// This node's consensus member identifier.
    pub member_id: MemberId,
    /// Address peers use to reach this node's internal RPC listener.
    pub advertise_address: String,
    /// Directory holding the consensus log, state, and snapshots.
    pub directory: PathBuf,
    /// Cluster name recorded in consensus metadata.
    pub cluster_name: String,
    /// Heartbeat interval in milliseconds.
    pub heartbeat_interval_millis: u64,
    /// Minimum election timeout in milliseconds.
    pub election_timeout_min_millis: u64,
    /// Maximum election timeout in milliseconds.
    pub election_timeout_max_millis: u64,
    /// Log entries appended before a snapshot is built.
    pub snapshot_logs_threshold: u64,
    /// Entries retained after a snapshot, for follower catch-up.
    pub retained_logs: u64,
    /// Time a forwarded or barrier operation may take.
    pub operation_timeout: Duration,
}

impl ConsensusSettings {
    /// Creates settings with conservative defaults for one node.
    #[must_use]
    pub fn new(
        member_id: MemberId,
        advertise_address: impl Into<String>,
        directory: impl AsRef<Path>,
    ) -> Self {
        Self {
            member_id,
            advertise_address: advertise_address.into(),
            directory: directory.as_ref().to_path_buf(),
            cluster_name: "record-store".to_owned(),
            heartbeat_interval_millis: 250,
            election_timeout_min_millis: 1_000,
            election_timeout_max_millis: 2_000,
            snapshot_logs_threshold: 8_192,
            retained_logs: 2_048,
            operation_timeout: Duration::from_secs(15),
        }
    }

    fn validate(&self) -> Result<(), ConsensusError> {
        if self.member_id == 0 {
            return Err(ConsensusError::Configuration(
                "consensus member identifier must be greater than zero".into(),
            ));
        }
        if self.advertise_address.trim().is_empty() {
            return Err(ConsensusError::Configuration(
                "consensus advertise address must not be empty".into(),
            ));
        }
        if self.heartbeat_interval_millis == 0
            || self.election_timeout_min_millis <= self.heartbeat_interval_millis * 2
            || self.election_timeout_max_millis <= self.election_timeout_min_millis
        {
            return Err(ConsensusError::Configuration(
                "election timeouts must exceed twice the heartbeat interval and be ordered".into(),
            ));
        }
        if self.snapshot_logs_threshold == 0 {
            return Err(ConsensusError::Configuration(
                "snapshot_logs_threshold must be greater than zero so the log is compacted".into(),
            ));
        }
        Ok(())
    }

    fn raft_config(&self) -> Result<Config, ConsensusError> {
        Config {
            cluster_name: self.cluster_name.clone(),
            heartbeat_interval: self.heartbeat_interval_millis,
            election_timeout_min: self.election_timeout_min_millis,
            election_timeout_max: self.election_timeout_max_millis,
            snapshot_policy: openraft::SnapshotPolicy::LogsSinceLast(self.snapshot_logs_threshold),
            max_in_snapshot_log_to_keep: self.retained_logs,
            ..Config::default()
        }
        .validate()
        .map_err(|error| ConsensusError::Configuration(error.to_string()))
    }
}

/// A single cluster member's view of the metadata consensus group.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ConsensusMemberStatus {
    /// Consensus member identifier.
    pub member_id: MemberId,
    /// Advertised internal address.
    pub address: String,
    /// Whether the member votes.
    pub voter: bool,
    /// Whether the leader currently has replication contact with the member.
    pub reachable: bool,
}

/// Health and leadership of the metadata consensus group.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MetadataQuorum {
    /// Derived quorum status.
    pub status: QuorumStatus,
    /// This member's role.
    pub role: String,
    /// This member's identifier.
    pub member_id: MemberId,
    /// The log position this member has applied.
    pub applied_index: Option<u64>,
    /// The last log position this member has stored.
    pub last_log_index: Option<u64>,
    /// The snapshot position this member holds.
    pub snapshot_index: Option<u64>,
    /// Every known member.
    pub members: Vec<ConsensusMemberStatus>,
}

/// The metadata consensus boundary used by the rest of Record Store.
pub struct MetadataConsensus {
    raft: Raft<RecordStoreTypeConfig>,
    state: Arc<ReplicatedState>,
    settings: ConsensusSettings,
    forwarder: Arc<tokio::sync::RwLock<Option<Arc<dyn LeaderForwarder>>>>,
}

impl MetadataConsensus {
    /// Starts consensus for this node.
    ///
    /// Starting does not join or create a cluster: the caller decides whether to
    /// initialize a new cluster or wait to be added by an existing one.
    pub async fn start<N>(
        settings: ConsensusSettings,
        network: N,
    ) -> Result<Arc<Self>, ConsensusError>
    where
        N: RaftNetworkFactory<RecordStoreTypeConfig>,
    {
        settings.validate()?;
        let config = Arc::new(settings.raft_config()?);
        let log_store = RedbLogStore::open(settings.directory.join("consensus-log.redb")).await?;
        let state = ReplicatedState::open(
            settings.directory.join("consensus-state.redb"),
            settings.directory.join("snapshots"),
        )
        .await?;
        let state_machine = StateMachineStore::new(Arc::clone(&state));
        let raft = Raft::new(
            settings.member_id,
            config,
            network,
            log_store,
            state_machine,
        )
        .await
        .map_err(|error| ConsensusError::Stopped(error.to_string()))?;
        info!(
            member = settings.member_id,
            address = %settings.advertise_address,
            "metadata consensus started"
        );
        Ok(Arc::new(Self {
            raft,
            state,
            settings,
            forwarder: Arc::new(tokio::sync::RwLock::new(None)),
        }))
    }

    /// Installs the transport used to reach the current leader.
    pub async fn set_leader_forwarder(&self, forwarder: Arc<dyn LeaderForwarder>) {
        *self.forwarder.write().await = Some(forwarder);
    }

    /// Returns the shared replicated state for local reads.
    #[must_use]
    pub fn state(&self) -> Arc<ReplicatedState> {
        Arc::clone(&self.state)
    }

    /// Returns this node's consensus member identifier.
    #[must_use]
    pub const fn member_id(&self) -> MemberId {
        self.settings.member_id
    }

    /// Returns whether the consensus group has been formed.
    pub async fn is_initialized(&self) -> Result<bool, ConsensusError> {
        self.raft
            .is_initialized()
            .await
            .map_err(|error| ConsensusError::Stopped(error.to_string()))
    }

    /// Forms a new single-member consensus group around this node.
    ///
    /// Additional members are added later through [`Self::add_member`], which is
    /// how a cluster grows without ever running two independent groups.
    pub async fn initialize_single_member(&self) -> Result<(), ConsensusError> {
        let mut members = BTreeMap::new();
        members.insert(
            self.settings.member_id,
            MemberNode {
                addr: self.settings.advertise_address.clone(),
            },
        );
        match self.raft.initialize(members).await {
            Ok(()) => Ok(()),
            Err(RaftError::APIError(InitializeError::NotAllowed(_))) => Ok(()),
            Err(error) => Err(ConsensusError::Internal(error.to_string())),
        }
    }

    /// Adds a member as a learner and optionally promotes it to a voter.
    ///
    /// Learners replicate the full metadata state without affecting quorum, so a
    /// cluster can grow past its voter target without weakening fault tolerance.
    pub async fn add_member(
        &self,
        member_id: MemberId,
        address: String,
        voter: bool,
    ) -> Result<(), ConsensusError> {
        let node = MemberNode { addr: address };
        match self.raft.add_learner(member_id, node, true).await {
            Ok(_) => {}
            Err(RaftError::APIError(ClientWriteError::ForwardToLeader(forward))) => {
                return Err(Self::forward_error(&forward));
            }
            Err(RaftError::APIError(ClientWriteError::ChangeMembershipError(error))) => {
                debug!(%error, "learner already present");
            }
            Err(error) => return Err(ConsensusError::Internal(error.to_string())),
        }
        if voter {
            self.promote_member(member_id).await?;
        }
        Ok(())
    }

    /// Promotes an existing learner to a voting member.
    pub async fn promote_member(&self, member_id: MemberId) -> Result<(), ConsensusError> {
        let mut voters = self.voter_ids().await?;
        if voters.contains(&member_id) {
            return Ok(());
        }
        voters.push(member_id);
        self.set_voters(voters).await
    }

    /// Demotes a voting member to a learner.
    pub async fn demote_member(&self, member_id: MemberId) -> Result<(), ConsensusError> {
        let voters: Vec<_> = self
            .voter_ids()
            .await?
            .into_iter()
            .filter(|candidate| *candidate != member_id)
            .collect();
        if voters.is_empty() {
            return Err(ConsensusError::Configuration(
                "refusing to remove the last metadata voter: the cluster would lose its \
                 metadata authority"
                    .into(),
            ));
        }
        self.set_voters(voters).await
    }

    /// Removes a member from the consensus group entirely.
    pub async fn remove_member(&self, member_id: MemberId) -> Result<(), ConsensusError> {
        self.demote_member(member_id).await.or_else(|error| {
            if matches!(error, ConsensusError::Configuration(_)) {
                Err(error)
            } else {
                Ok(())
            }
        })?;
        match self
            .raft
            .change_membership(
                openraft::ChangeMembers::RemoveNodes([member_id].into_iter().collect()),
                false,
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(RaftError::APIError(ClientWriteError::ForwardToLeader(forward))) => {
                Err(Self::forward_error(&forward))
            }
            Err(RaftError::APIError(ClientWriteError::ChangeMembershipError(error))) => {
                debug!(%error, "member already removed");
                Ok(())
            }
            Err(error) => Err(ConsensusError::Internal(error.to_string())),
        }
    }

    async fn set_voters(&self, voters: Vec<MemberId>) -> Result<(), ConsensusError> {
        let voters: std::collections::BTreeSet<_> = voters.into_iter().collect();
        match self.raft.change_membership(voters, true).await {
            Ok(_) => Ok(()),
            Err(RaftError::APIError(ClientWriteError::ForwardToLeader(forward))) => {
                Err(Self::forward_error(&forward))
            }
            Err(RaftError::APIError(ClientWriteError::ChangeMembershipError(error))) => {
                Err(ConsensusError::Configuration(error.to_string()))
            }
            Err(error) => Err(ConsensusError::Internal(error.to_string())),
        }
    }

    async fn voter_ids(&self) -> Result<Vec<MemberId>, ConsensusError> {
        let metrics = self.raft.metrics().borrow().clone();
        Ok(metrics.membership_config.membership().voter_ids().collect())
    }

    /// Proposes a replicated write, forwarding to the leader when necessary.
    pub async fn write(
        &self,
        command: ClusterWrite,
    ) -> Result<ClusterWriteResponse, ConsensusError> {
        match self.raft.client_write(command.clone()).await {
            Ok(response) => Ok(response.data),
            Err(RaftError::APIError(ClientWriteError::ForwardToLeader(forward))) => {
                self.forward_write(&forward, &command).await
            }
            Err(RaftError::APIError(ClientWriteError::ChangeMembershipError(error))) => {
                Err(ConsensusError::Configuration(error.to_string()))
            }
            Err(RaftError::Fatal(fatal)) => Err(Self::fatal_error(&fatal)),
        }
    }

    /// Ensures a subsequent local read observes every committed write.
    ///
    /// On the leader this confirms leadership with a quorum and waits for the
    /// state machine to catch up. On a follower it asks the leader for the log
    /// position to wait for, which is a read-index barrier rather than a
    /// weakened consistency level.
    pub async fn ensure_read_consistency(&self) -> Result<(), ConsensusError> {
        match self.raft.ensure_linearizable().await {
            Ok(_) => Ok(()),
            Err(RaftError::APIError(CheckIsLeaderError::ForwardToLeader(forward))) => {
                self.forward_read_barrier(&forward).await
            }
            Err(RaftError::APIError(CheckIsLeaderError::QuorumNotEnough(error))) => {
                Err(ConsensusError::QuorumUnavailable(error.to_string()))
            }
            Err(RaftError::Fatal(fatal)) => Err(Self::fatal_error(&fatal)),
        }
    }

    /// Returns the log position a linearizable read must observe.
    ///
    /// This is the leader-side half of the read barrier.
    pub async fn read_barrier_index(&self) -> Result<Option<u64>, ConsensusError> {
        match self.raft.ensure_linearizable().await {
            Ok(log_id) => Ok(log_id.map(|log_id| log_id.index)),
            Err(RaftError::APIError(CheckIsLeaderError::ForwardToLeader(forward))) => {
                Err(Self::forward_error(&forward))
            }
            Err(RaftError::APIError(CheckIsLeaderError::QuorumNotEnough(error))) => {
                Err(ConsensusError::QuorumUnavailable(error.to_string()))
            }
            Err(RaftError::Fatal(fatal)) => Err(Self::fatal_error(&fatal)),
        }
    }

    async fn forward_write(
        &self,
        forward: &ForwardToLeader<MemberId, MemberNode>,
        command: &ClusterWrite,
    ) -> Result<ClusterWriteResponse, ConsensusError> {
        let (leader, address) = Self::leader_address(forward)?;
        let forwarder = self.forwarder.read().await.clone();
        let Some(forwarder) = forwarder else {
            return Err(ConsensusError::NotLeader { leader, address });
        };
        debug!(
            leader,
            command = command.name(),
            "forwarding metadata write"
        );
        forwarder.forward_write(leader, &address, command).await
    }

    async fn forward_read_barrier(
        &self,
        forward: &ForwardToLeader<MemberId, MemberNode>,
    ) -> Result<(), ConsensusError> {
        let (leader, address) = Self::leader_address(forward)?;
        let forwarder = self.forwarder.read().await.clone();
        let Some(forwarder) = forwarder else {
            return Err(ConsensusError::NotLeader { leader, address });
        };
        let index = forwarder.forward_read_barrier(leader, &address).await?;
        if let Some(index) = index {
            self.raft
                .wait(Some(self.settings.operation_timeout))
                .applied_index_at_least(Some(index), "metadata read barrier")
                .await
                .map_err(|error| ConsensusError::QuorumUnavailable(error.to_string()))?;
        }
        Ok(())
    }

    fn leader_address(
        forward: &ForwardToLeader<MemberId, MemberNode>,
    ) -> Result<(MemberId, String), ConsensusError> {
        match (forward.leader_id, forward.leader_node.as_ref()) {
            (Some(leader), Some(node)) => Ok((leader, node.addr.clone())),
            _ => Err(ConsensusError::NoLeader),
        }
    }

    fn forward_error(forward: &ForwardToLeader<MemberId, MemberNode>) -> ConsensusError {
        match Self::leader_address(forward) {
            Ok((leader, address)) => ConsensusError::NotLeader { leader, address },
            Err(error) => error,
        }
    }

    fn fatal_error(fatal: &Fatal<MemberId>) -> ConsensusError {
        ConsensusError::Stopped(fatal.to_string())
    }

    /// Returns whether this member is currently the leader.
    pub async fn is_leader(&self) -> bool {
        let metrics = self.raft.metrics().borrow().clone();
        metrics.state == ServerState::Leader && metrics.current_leader == Some(self.member_id())
    }

    /// Returns the current leader's member identifier and address.
    pub async fn current_leader(&self) -> Option<(MemberId, String)> {
        let metrics = self.raft.metrics().borrow().clone();
        let leader = metrics.current_leader?;
        let address = metrics
            .membership_config
            .membership()
            .get_node(&leader)
            .map(|node| node.addr.clone())
            .unwrap_or_default();
        Some((leader, address))
    }

    /// Waits until a leader is elected or the timeout elapses.
    pub async fn wait_for_leader(&self, timeout: Duration) -> Result<MemberId, ConsensusError> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut metrics = self.raft.metrics();
        loop {
            if let Some(leader) = metrics.borrow().current_leader {
                return Ok(leader);
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(ConsensusError::NoLeader);
            }
            if tokio::time::timeout(remaining, metrics.changed())
                .await
                .is_err()
            {
                return Err(ConsensusError::NoLeader);
            }
        }
    }

    /// Returns the current quorum and leadership view.
    pub async fn quorum(&self) -> MetadataQuorum {
        let metrics: RaftMetrics<MemberId, MemberNode> = self.raft.metrics().borrow().clone();
        let membership = metrics.membership_config.membership();
        let voters: Vec<MemberId> = membership.voter_ids().collect();
        let replication = metrics.replication.clone().unwrap_or_default();
        let mut members = Vec::new();
        for (member_id, node) in membership.nodes() {
            let voter = voters.contains(member_id);
            let reachable = if *member_id == metrics.id {
                true
            } else if metrics.state == ServerState::Leader {
                replication.get(member_id).is_some_and(Option::is_some)
            } else {
                // Only the leader observes replication health directly.
                metrics.current_leader == Some(*member_id)
            };
            members.push(ConsensusMemberStatus {
                member_id: *member_id,
                address: node.addr.clone(),
                voter,
                reachable,
            });
        }
        let voter_count = u32::try_from(voters.len()).unwrap_or(u32::MAX);
        let healthy_voters = u32::try_from(
            members
                .iter()
                .filter(|member| member.voter && member.reachable)
                .count(),
        )
        .unwrap_or(u32::MAX);
        let leader_label = metrics.current_leader.map(|leader| {
            membership
                .get_node(&leader)
                .map_or_else(|| leader.to_string(), |node| node.addr.clone())
        });
        let mut status = QuorumStatus::evaluate(voter_count, healthy_voters, leader_label);
        if metrics.state != ServerState::Leader && metrics.current_leader.is_none() {
            status.writable = false;
            status.health = status.health.worst(ClusterHealth::Unavailable);
        }
        MetadataQuorum {
            status,
            role: server_state_name(metrics.state).to_owned(),
            member_id: metrics.id,
            applied_index: metrics.last_applied.map(|log_id| log_id.index),
            last_log_index: metrics.last_log_index,
            snapshot_index: metrics.snapshot.map(|log_id| log_id.index),
            members,
        }
    }

    /// Handles an append-entries request received over internal RPC.
    pub async fn handle_append_entries(
        &self,
        request: AppendEntriesRequest<RecordStoreTypeConfig>,
    ) -> Result<AppendEntriesResponse<MemberId>, ConsensusError> {
        self.raft
            .append_entries(request)
            .await
            .map_err(|error| ConsensusError::Internal(error.to_string()))
    }

    /// Handles a vote request received over internal RPC.
    pub async fn handle_vote(
        &self,
        request: VoteRequest<MemberId>,
    ) -> Result<VoteResponse<MemberId>, ConsensusError> {
        self.raft
            .vote(request)
            .await
            .map_err(|error| ConsensusError::Internal(error.to_string()))
    }

    /// Handles a snapshot installation chunk received over internal RPC.
    pub async fn handle_install_snapshot(
        &self,
        request: InstallSnapshotRequest<RecordStoreTypeConfig>,
    ) -> Result<InstallSnapshotResponse<MemberId>, ConsensusError> {
        self.raft
            .install_snapshot(request)
            .await
            .map_err(|error| ConsensusError::Internal(error.to_string()))
    }

    /// Requests an immediate snapshot, used by administrative tooling.
    pub async fn trigger_snapshot(&self) -> Result<(), ConsensusError> {
        self.raft
            .trigger()
            .snapshot()
            .await
            .map_err(|error| ConsensusError::Stopped(error.to_string()))
    }

    /// Stops the consensus engine.
    pub async fn shutdown(&self) {
        if let Err(error) = self.raft.shutdown().await {
            warn!(%error, "metadata consensus shutdown reported an error");
        }
    }
}

const fn server_state_name(state: ServerState) -> &'static str {
    match state {
        ServerState::Learner => "learner",
        ServerState::Follower => "follower",
        ServerState::Candidate => "candidate",
        ServerState::Leader => "leader",
        ServerState::Shutdown => "shutdown",
    }
}

/// Turns a rejection into a consensus error, for callers that only want errors.
#[must_use]
pub fn rejection_error(kind: RejectionKind, message: impl Into<String>) -> ConsensusError {
    ConsensusError::Rejected(CommandRejection {
        kind,
        message: message.into(),
    })
}
