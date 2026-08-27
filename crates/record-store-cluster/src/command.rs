//! Deterministic cluster-state commands.
//!
//! Every mutation of replicated cluster state is expressed as one command. The
//! commands carry all non-deterministic inputs, such as timestamps and generated
//! identifiers, so that applying the same ordered command sequence on any member
//! produces byte-identical state. That property is what allows the cluster
//! catalog to be a consensus state machine rather than a shared database.

use chrono::{DateTime, Utc};
use record_store_core::{
    Checksum, ClusterId, ClusterOperationId, JoinTokenId, NodeId, ObjectId, ReplicaTaskId,
};
use serde::{Deserialize, Serialize};

use crate::{
    config::ClusterConfig,
    credentials::{JoinToken, NodeCredential},
    identity::RaftNodeId,
    replica::{PayloadPlacement, Replica, ReplicaState},
    tasks::{
        ClusterOperation, ClusterOperationState, OperationProgress, ReplicaTask, ReplicaTaskKind,
    },
    topology::{
        FailureDomain, NodeActivity, NodeCapacity, NodeRecord, NodeRegistration, NodeState,
        StorageClass,
    },
    version::NodeVersions,
};

/// Immutable cluster identity established by `record-store cluster init`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterIdentity {
    /// Stable cluster identifier.
    pub cluster_id: ClusterId,
    /// Durable cluster-catalog layout version.
    pub cluster_format_version: u32,
    /// Time the cluster was initialized.
    pub created_at: DateTime<Utc>,
}

/// One deterministic mutation of replicated cluster state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum ClusterCommand {
    /// Establish the cluster identity and its initial configuration.
    InitializeCluster {
        /// Identity to persist.
        identity: ClusterIdentity,
        /// Initial cluster-wide configuration.
        config: Box<ClusterConfig>,
    },
    /// Replace the cluster-wide configuration.
    UpdateConfig {
        /// Validated replacement configuration.
        config: Box<ClusterConfig>,
        /// Time of the change.
        at: DateTime<Utc>,
    },
    /// Register a node, assigning a consensus member identifier if it is new.
    RegisterNode {
        /// What the node reported about itself.
        registration: Box<NodeRegistration>,
        /// Time of registration.
        at: DateTime<Utc>,
    },
    /// Record a heartbeat's capacity and activity report.
    Heartbeat {
        /// Reporting node.
        node_id: NodeId,
        /// Capacity measured by the node.
        capacity: NodeCapacity,
        /// Bounded activity counters.
        activity: NodeActivity,
        /// Heartbeat time.
        at: DateTime<Utc>,
    },
    /// Update the addressing and labelling a node advertises.
    UpdateNodeDescriptor {
        /// Node being updated.
        node_id: NodeId,
        /// Internal RPC address.
        rpc_address: String,
        /// Optional client-facing S3 endpoint.
        s3_endpoint: Option<String>,
        /// Advertised versions.
        versions: Box<NodeVersions>,
        /// Storage class.
        storage_class: StorageClass,
        /// Topology labels.
        failure_domain: FailureDomain,
        /// Process start time.
        started_at: DateTime<Utc>,
        /// Time of the update.
        at: DateTime<Utc>,
    },
    /// Apply a validated node lifecycle transition.
    SetNodeState {
        /// Node being transitioned.
        node_id: NodeId,
        /// Requested state.
        state: NodeState,
        /// Operator-facing reason.
        reason: Option<String>,
        /// Time of the transition.
        at: DateTime<Utc>,
    },
    /// Change whether a node votes in the metadata consensus group.
    SetNodeMetadataVoter {
        /// Node being changed.
        node_id: NodeId,
        /// Whether the node should vote.
        voter: bool,
        /// Time of the change.
        at: DateTime<Utc>,
    },
    /// Remove a node record entirely, after decommissioning.
    ForgetNode {
        /// Node to forget.
        node_id: NodeId,
    },
    /// Commit placement metadata for a payload.
    PutPlacement {
        /// Placement to commit.
        placement: Box<PayloadPlacement>,
    },
    /// Insert or replace one replica record.
    UpsertReplica {
        /// Payload the replica belongs to.
        object_id: ObjectId,
        /// Replica record.
        replica: Box<Replica>,
        /// Time of the change.
        at: DateTime<Utc>,
    },
    /// Change a replica's state, for example after verification found damage.
    SetReplicaState {
        /// Payload the replica belongs to.
        object_id: ObjectId,
        /// Node holding the replica.
        node_id: NodeId,
        /// New replica state.
        state: ReplicaState,
        /// Checksum the node observed, when it reported one.
        checksum: Option<Checksum>,
        /// Whether this update counts as a successful verification.
        verified: bool,
        /// Time of the change.
        at: DateTime<Utc>,
    },
    /// Remove a replica record after its bytes were released.
    RemoveReplica {
        /// Payload the replica belonged to.
        object_id: ObjectId,
        /// Node that released the bytes.
        node_id: NodeId,
        /// Time of the change.
        at: DateTime<Utc>,
    },
    /// Change the desired replica count for one payload.
    SetDesiredReplicas {
        /// Payload to change.
        object_id: ObjectId,
        /// New desired replica count.
        desired: u8,
        /// Time of the change.
        at: DateTime<Utc>,
    },
    /// Delete placement metadata and create a durable tombstone.
    DeletePlacement {
        /// Payload to delete everywhere.
        object_id: ObjectId,
        /// Time of the deletion.
        at: DateTime<Utc>,
    },
    /// Record that a node removed its copy of a tombstoned payload.
    AcknowledgeTombstone {
        /// Tombstoned payload.
        object_id: ObjectId,
        /// Node confirming removal.
        node_id: NodeId,
        /// Time of the acknowledgement.
        at: DateTime<Utc>,
    },
    /// Purge a fully acknowledged tombstone past its retention window.
    PurgeTombstone {
        /// Tombstoned payload.
        object_id: ObjectId,
    },
    /// Enqueue a replica movement task, ignoring duplicates.
    EnqueueTask {
        /// Task to enqueue.
        task: Box<ReplicaTask>,
    },
    /// Claim a queued task for execution under a lease.
    ClaimTask {
        /// Task to claim.
        task_id: ReplicaTaskId,
        /// Node claiming the task.
        node_id: NodeId,
        /// Lease duration.
        lease_seconds: u64,
        /// Claim time.
        at: DateTime<Utc>,
    },
    /// Mark a task completed.
    CompleteTask {
        /// Task that finished.
        task_id: ReplicaTaskId,
        /// Completion time.
        at: DateTime<Utc>,
    },
    /// Record a task failure, parking it once retries are exhausted.
    FailTask {
        /// Task that failed.
        task_id: ReplicaTaskId,
        /// Failure message.
        reason: String,
        /// Attempts permitted before parking.
        maximum_attempts: u32,
        /// Failure time.
        at: DateTime<Utc>,
    },
    /// Return a task to the queue, for example after a lease expired.
    RequeueTask {
        /// Task to requeue.
        task_id: ReplicaTaskId,
        /// Optional reason.
        reason: Option<String>,
        /// Requeue time.
        at: DateTime<Utc>,
    },
    /// Cancel a task that is no longer needed.
    CancelTask {
        /// Task to cancel.
        task_id: ReplicaTaskId,
        /// Cancellation reason.
        reason: String,
        /// Cancellation time.
        at: DateTime<Utc>,
    },
    /// Remove a finished task record.
    PurgeTask {
        /// Task to remove.
        task_id: ReplicaTaskId,
    },
    /// Start a long-running cluster operation.
    StartOperation {
        /// Operation record.
        operation: Box<ClusterOperation>,
    },
    /// Update a long-running operation's state and progress.
    UpdateOperation {
        /// Operation to update.
        operation_id: ClusterOperationId,
        /// New lifecycle state.
        state: ClusterOperationState,
        /// Progress counters.
        progress: OperationProgress,
        /// Operator-facing message.
        message: Option<String>,
        /// Update time.
        at: DateTime<Utc>,
    },
    /// Store a newly issued join token.
    IssueJoinToken {
        /// Token record without its secret.
        token: Box<JoinToken>,
    },
    /// Record a successful use of a join token.
    ConsumeJoinToken {
        /// Token that was used.
        token_id: JoinTokenId,
        /// Use time.
        at: DateTime<Utc>,
    },
    /// Revoke a join token.
    RevokeJoinToken {
        /// Token to revoke.
        token_id: JoinTokenId,
        /// Revocation time.
        at: DateTime<Utc>,
    },
    /// Remove an unusable join token record.
    PurgeJoinToken {
        /// Token to remove.
        token_id: JoinTokenId,
    },
    /// Store or replace a node's internal RPC credential.
    PutNodeCredential {
        /// Credential record without its secret.
        credential: Box<NodeCredential>,
    },
    /// Enable or disable a node credential.
    SetNodeCredentialDisabled {
        /// Node whose credential changes.
        node_id: NodeId,
        /// Whether the credential is disabled.
        disabled: bool,
        /// Time of the change.
        at: DateTime<Utc>,
    },
}

impl ClusterCommand {
    /// Returns a stable short name for tracing, metrics, and audit records.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::InitializeCluster { .. } => "initialize_cluster",
            Self::UpdateConfig { .. } => "update_config",
            Self::RegisterNode { .. } => "register_node",
            Self::Heartbeat { .. } => "heartbeat",
            Self::UpdateNodeDescriptor { .. } => "update_node_descriptor",
            Self::SetNodeState { .. } => "set_node_state",
            Self::SetNodeMetadataVoter { .. } => "set_node_metadata_voter",
            Self::ForgetNode { .. } => "forget_node",
            Self::PutPlacement { .. } => "put_placement",
            Self::UpsertReplica { .. } => "upsert_replica",
            Self::SetReplicaState { .. } => "set_replica_state",
            Self::RemoveReplica { .. } => "remove_replica",
            Self::SetDesiredReplicas { .. } => "set_desired_replicas",
            Self::DeletePlacement { .. } => "delete_placement",
            Self::AcknowledgeTombstone { .. } => "acknowledge_tombstone",
            Self::PurgeTombstone { .. } => "purge_tombstone",
            Self::EnqueueTask { .. } => "enqueue_task",
            Self::ClaimTask { .. } => "claim_task",
            Self::CompleteTask { .. } => "complete_task",
            Self::FailTask { .. } => "fail_task",
            Self::RequeueTask { .. } => "requeue_task",
            Self::CancelTask { .. } => "cancel_task",
            Self::PurgeTask { .. } => "purge_task",
            Self::StartOperation { .. } => "start_operation",
            Self::UpdateOperation { .. } => "update_operation",
            Self::IssueJoinToken { .. } => "issue_join_token",
            Self::ConsumeJoinToken { .. } => "consume_join_token",
            Self::RevokeJoinToken { .. } => "revoke_join_token",
            Self::PurgeJoinToken { .. } => "purge_join_token",
            Self::PutNodeCredential { .. } => "put_node_credential",
            Self::SetNodeCredentialDisabled { .. } => "set_node_credential_disabled",
        }
    }
}

/// Result of applying one cluster command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "value", rename_all = "snake_case")]
pub enum ClusterOutcome {
    /// The command produced no value.
    None,
    /// A node record after the command was applied.
    Node(Box<NodeRecord>),
    /// A node registration result.
    Registration {
        /// Resulting node record.
        record: Box<NodeRecord>,
        /// Consensus member identifier assigned to the node.
        raft_id: RaftNodeId,
        /// Whether the node was newly created rather than re-registered.
        created: bool,
    },
    /// Placement metadata after the command was applied.
    Placement(Box<PayloadPlacement>),
    /// A replica movement task after the command was applied.
    Task(Box<ReplicaTask>),
    /// A long-running operation after the command was applied.
    Operation(Box<ClusterOperation>),
    /// The cluster configuration after the command was applied.
    Config(Box<ClusterConfig>),
    /// The cluster identity.
    Identity(Box<ClusterIdentity>),
    /// A boolean result, such as whether anything actually changed.
    Changed(bool),
}

impl ClusterOutcome {
    /// Returns the node record, if the command produced one.
    #[must_use]
    pub fn node(self) -> Option<NodeRecord> {
        match self {
            Self::Node(record) => Some(*record),
            Self::Registration { record, .. } => Some(*record),
            _ => None,
        }
    }

    /// Returns the placement record, if the command produced one.
    #[must_use]
    pub fn placement(self) -> Option<PayloadPlacement> {
        match self {
            Self::Placement(placement) => Some(*placement),
            _ => None,
        }
    }

    /// Returns the task record, if the command produced one.
    #[must_use]
    pub fn task(self) -> Option<ReplicaTask> {
        match self {
            Self::Task(task) => Some(*task),
            _ => None,
        }
    }

    /// Returns whether a command that reports change actually changed state.
    #[must_use]
    pub const fn changed(&self) -> bool {
        matches!(self, Self::Changed(true))
    }
}

/// Convenience constructor for the common repair enqueue command.
#[must_use]
pub fn enqueue_repair(
    object_id: ObjectId,
    kind: ReplicaTaskKind,
    healthy: u32,
    desired: u32,
    size: u64,
    at: DateTime<Utc>,
) -> ClusterCommand {
    let priority = crate::tasks::ReplicaTaskPriority::classify(kind, healthy, desired);
    ClusterCommand::EnqueueTask {
        task: Box::new(ReplicaTask::queued(object_id, kind, priority, size, at)),
    }
}
