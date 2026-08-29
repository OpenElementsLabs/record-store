//! Deterministic cluster-state commands.
//!
//! Every mutation of replicated cluster state is expressed as one command. The
//! commands carry all non-deterministic inputs, such as timestamps and generated
//! identifiers, so that applying the same ordered command sequence on any member
//! produces byte-identical state. That property is what allows the cluster
//! catalog to be a consensus state machine rather than a shared database.

use chrono::{DateTime, Utc};
use record_store_core::{
    Checksum, ClusterId, ClusterOperationId, DeviceId, JoinTokenId, NodeId, ObjectId, ReplicaTaskId,
};
use serde::{Deserialize, Serialize};

use crate::{
    config::ClusterConfig,
    credentials::{JoinToken, NodeCredential},
    device::{DeviceRecord, DeviceState},
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
    /// Register or refresh an explicitly configured device.
    RegisterDevice {
        /// Node serving the device.
        node_id: NodeId,
        /// Device record supplied by that node or an administrator.
        device: Box<DeviceRecord>,
        /// Time of registration.
        at: DateTime<Utc>,
    },
    /// Apply a validated device lifecycle transition.
    /// Defines or replaces a storage policy.
    PutStoragePolicy {
        /// Policy being committed. Its class is its identity.
        policy: Box<crate::policy::StoragePolicy>,
        /// Commit time.
        at: DateTime<Utc>,
    },
    /// Removes a storage policy.
    DeleteStoragePolicy {
        /// Class being removed.
        class: crate::topology::StorageClass,
        /// Commit time.
        at: DateTime<Utc>,
    },
    SetDeviceState {
        /// Node serving the device.
        node_id: NodeId,
        /// Device being transitioned.
        device_id: DeviceId,
        /// Requested lifecycle state.
        state: DeviceState,
        /// Time of transition.
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
    /// Change a replica's state on one exact device.
    SetReplicaStateOnDevice {
        /// Payload the replica belongs to.
        object_id: ObjectId,
        /// Node holding the replica.
        node_id: NodeId,
        /// Device holding the replica.
        device_id: DeviceId,
        /// New replica state.
        state: ReplicaState,
        /// Checksum the device observed, when it reported one.
        checksum: Option<Checksum>,
        /// Whether this update counts as successful verification.
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
    /// Remove one exact device replica after its bytes were released.
    RemoveReplicaFromDevice {
        /// Payload the replica belonged to.
        object_id: ObjectId,
        /// Node that released the bytes.
        node_id: NodeId,
        /// Device that released the bytes.
        device_id: DeviceId,
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
            Self::RegisterDevice { .. } => "register_device",
            Self::PutStoragePolicy { .. } => "put_storage_policy",
            Self::DeleteStoragePolicy { .. } => "delete_storage_policy",
            Self::SetDeviceState { .. } => "set_device_state",
            Self::SetNodeMetadataVoter { .. } => "set_node_metadata_voter",
            Self::ForgetNode { .. } => "forget_node",
            Self::PutPlacement { .. } => "put_placement",
            Self::UpsertReplica { .. } => "upsert_replica",
            Self::SetReplicaState { .. } => "set_replica_state",
            Self::SetReplicaStateOnDevice { .. } => "set_replica_state_on_device",
            Self::RemoveReplica { .. } => "remove_replica",
            Self::RemoveReplicaFromDevice { .. } => "remove_replica_from_device",
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
    /// A device record after the command was applied.
    Device(Box<DeviceRecord>),
    /// A committed storage policy.
    StoragePolicy(Box<crate::policy::StoragePolicy>),
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

    /// Returns the device record, if the command produced one.
    #[must_use]
    pub fn device(self) -> Option<DeviceRecord> {
        match self {
            Self::Device(device) => Some(*device),
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

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use record_store_core::{NodeId, ObjectId, ReplicaTaskId};

    use super::*;
    use crate::catalog::test_support::{identity, registration};
    use crate::tasks::{ReplicaTaskKind, ReplicaTaskPriority, ReplicaTaskState};

    fn sample_commands() -> Vec<(ClusterCommand, &'static str)> {
        let now = Utc::now();
        vec![
            (
                ClusterCommand::InitializeCluster {
                    identity: identity(),
                    config: Box::new(ClusterConfig::default()),
                },
                "initialize_cluster",
            ),
            (
                ClusterCommand::UpdateConfig {
                    config: Box::new(ClusterConfig::default()),
                    at: now,
                },
                "update_config",
            ),
            (
                ClusterCommand::RegisterNode {
                    registration: Box::new(registration()),
                    at: now,
                },
                "register_node",
            ),
            (
                ClusterCommand::SetNodeState {
                    node_id: NodeId::new(),
                    state: crate::topology::NodeState::Draining,
                    reason: None,
                    at: now,
                },
                "set_node_state",
            ),
            (
                ClusterCommand::ForgetNode {
                    node_id: NodeId::new(),
                },
                "forget_node",
            ),
            (
                ClusterCommand::PurgeTask {
                    task_id: ReplicaTaskId::new(),
                },
                "purge_task",
            ),
        ]
    }

    /// The command name reaches tracing, metrics, and audit records. A rename
    /// silently breaks an operator's dashboards and their audit queries at once.
    #[test]
    fn every_sampled_command_reports_its_stable_name() {
        for (command, expected) in sample_commands() {
            assert_eq!(command.name(), expected);
        }
    }

    /// Commands cross the replication log, so each must survive the encoding a
    /// follower decodes it with and still be the same command afterwards.
    #[test]
    fn commands_round_trip_through_their_encoded_form() {
        for (command, expected) in sample_commands() {
            let encoded = serde_json::to_vec(&command).expect("serialise");
            let decoded: ClusterCommand = serde_json::from_slice(&encoded).expect("deserialise");
            assert_eq!(decoded.name(), expected);
        }
    }

    /// Each accessor answers for exactly one outcome shape. Returning a value
    /// for the wrong one would let a caller act on a record it never received.
    #[test]
    fn an_outcome_accessor_answers_only_for_its_own_shape() {
        let now = Utc::now();
        let record = crate::topology::NodeRecord::joining(registration(), 1, true, now);
        let task = ReplicaTask {
            id: ReplicaTaskId::new(),
            object_id: ObjectId::new(),
            kind: ReplicaTaskKind::Repair,
            priority: ReplicaTaskPriority::Low,
            source_node: None,
            source_device: None,
            target_node: None,
            target_device: None,
            operation_id: None,
            size: 0,
            state: ReplicaTaskState::Queued,
            attempts: 0,
            last_error: None,
            created_at: now,
            updated_at: now,
        };

        assert!(
            ClusterOutcome::Node(Box::new(record.clone()))
                .node()
                .is_some()
        );
        assert!(
            ClusterOutcome::Node(Box::new(record.clone()))
                .task()
                .is_none()
        );
        assert!(
            ClusterOutcome::Node(Box::new(record.clone()))
                .placement()
                .is_none()
        );
        assert!(ClusterOutcome::Task(Box::new(task)).task().is_some());
        assert!(ClusterOutcome::None.node().is_none());
    }

    /// A registration also yields the node record, because a joining node needs
    /// its own descriptor back in the same round trip.
    #[test]
    fn a_registration_outcome_still_yields_the_node_record() {
        let record = crate::topology::NodeRecord::joining(registration(), 1, true, Utc::now());
        let outcome = ClusterOutcome::Registration {
            record: Box::new(record.clone()),
            raft_id: 1,
            created: true,
        };
        assert_eq!(outcome.node().expect("record").node_id, record.node_id);
    }

    /// `changed` distinguishes a command that did something from one that was a
    /// no-op, which is what keeps an idempotent retry from looking like work.
    #[test]
    fn changed_is_true_only_for_a_command_that_altered_state() {
        assert!(ClusterOutcome::Changed(true).changed());
        assert!(!ClusterOutcome::Changed(false).changed());
        assert!(!ClusterOutcome::None.changed());
    }
}
