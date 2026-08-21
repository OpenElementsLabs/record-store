//! Durable cluster catalog.
//!
//! The catalog holds every piece of replicated cluster state: identity,
//! configuration, membership, replica placement, tombstones, movement tasks, and
//! long-running operations. Mutations are applied through transaction-scoped
//! functions so that a consensus state machine can commit a command and its
//! applied log position in one atomic transaction.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
};

use chrono::{DateTime, TimeDelta, Utc};
use oes_core::{
    ClusterId, ClusterOperationId, JoinTokenId, NodeCredentialId, NodeId, ObjectId, ReplicaTaskId,
};
use redb::{
    Database, ReadableTable, ReadableTableMetadata, TableDefinition, TableHandle, WriteTransaction,
};
use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;

use crate::{
    command::{ClusterCommand, ClusterIdentity, ClusterOutcome},
    config::{ClusterConfig, ClusterConfigError},
    credentials::{JoinToken, NodeCredential},
    identity::RaftNodeId,
    replica::{PayloadPlacement, ReplicaState, Tombstone},
    tasks::{ClusterOperation, ReplicaTask, ReplicaTaskKind, ReplicaTaskState},
    topology::{ClusterTopology, NodeRecord, NodeState, TopologyError},
    version::CLUSTER_FORMAT_VERSION,
};

const IDENTITY: TableDefinition<&[u8], &[u8]> = TableDefinition::new("cluster.identity.v1");
const CONFIG: TableDefinition<&[u8], &[u8]> = TableDefinition::new("cluster.config.v1");
const NODES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("cluster.nodes.v1");
const NODE_BY_MEMBER: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("cluster.node_by_member.v1");
const COUNTERS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("cluster.counters.v1");
const PLACEMENTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("cluster.placements.v1");
const NODE_REPLICAS: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("cluster.node_replicas.v1");
const TOMBSTONES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("cluster.tombstones.v1");
const TASKS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("cluster.tasks.v1");
const TASK_QUEUE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("cluster.task_queue.v1");
const TASK_BY_OBJECT: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("cluster.task_by_object.v1");
const OPERATIONS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("cluster.operations.v1");
const JOIN_TOKENS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("cluster.join_tokens.v1");
const NODE_CREDENTIALS: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("cluster.node_credentials.v1");
const SCHEMA: TableDefinition<&[u8], &[u8]> = TableDefinition::new("cluster.schema.v1");

/// Every cluster table, used by consensus snapshot export and import.
pub const CLUSTER_TABLES: &[TableDefinition<'static, &'static [u8], &'static [u8]>] = &[
    IDENTITY,
    CONFIG,
    NODES,
    NODE_BY_MEMBER,
    COUNTERS,
    PLACEMENTS,
    NODE_REPLICAS,
    TOMBSTONES,
    TASKS,
    TASK_QUEUE,
    TASK_BY_OBJECT,
    OPERATIONS,
    JOIN_TOKENS,
    NODE_CREDENTIALS,
    SCHEMA,
];

const SINGLETON: &[u8] = b"singleton";
const SCHEMA_VERSION_KEY: &[u8] = b"cluster_format_version";
const NEXT_MEMBER_ID: &[u8] = b"next_member_id";
const PLACEMENT_COUNT: &[u8] = b"placements";
const LOGICAL_BYTES: &[u8] = b"logical_bytes";
const PHYSICAL_BYTES: &[u8] = b"physical_bytes";
const TOMBSTONE_COUNT: &[u8] = b"tombstones";
const ACTIVE_TASKS: &[u8] = b"active_tasks";
const PARKED_TASKS: &[u8] = b"parked_tasks";
const UNDER_REPLICATED: &[u8] = b"under_replicated";
const UNAVAILABLE_PAYLOADS: &[u8] = b"unavailable_payloads";

/// Failures raised by the cluster catalog.
#[derive(Debug, Error)]
pub enum ClusterCatalogError {
    /// The cluster has not been initialized yet.
    #[error("cluster has not been initialized; run 'oes cluster init' first")]
    NotInitialized,
    /// The cluster was already initialized.
    #[error("cluster {0} is already initialized")]
    AlreadyInitialized(ClusterId),
    /// A command referred to a different cluster.
    #[error("command targets cluster {requested} but this catalog holds cluster {stored}")]
    ClusterMismatch {
        /// Cluster stored in the catalog.
        stored: ClusterId,
        /// Cluster named by the command.
        requested: ClusterId,
    },
    /// The node is unknown.
    #[error("node {0} is not a member of this cluster")]
    NodeNotFound(NodeId),
    /// Placement metadata is unknown.
    #[error("placement metadata for payload {0} was not found")]
    PlacementNotFound(ObjectId),
    /// The referenced replica record does not exist.
    #[error("payload {object_id} has no replica on node {node_id}")]
    ReplicaNotFound {
        /// Payload identifier.
        object_id: ObjectId,
        /// Node identifier.
        node_id: NodeId,
    },
    /// The tombstone is unknown.
    #[error("tombstone for payload {0} was not found")]
    TombstoneNotFound(ObjectId),
    /// The task is unknown.
    #[error("replica task {0} was not found")]
    TaskNotFound(ReplicaTaskId),
    /// The operation is unknown.
    #[error("cluster operation {0} was not found")]
    OperationNotFound(ClusterOperationId),
    /// The join token is unknown.
    #[error("join token {0} was not found")]
    JoinTokenNotFound(JoinTokenId),
    /// The node credential is unknown.
    #[error("node credential for node {0} was not found")]
    CredentialNotFound(NodeId),
    /// A node lifecycle transition was refused.
    #[error(transparent)]
    Topology(#[from] TopologyError),
    /// The proposed configuration was invalid.
    #[error(transparent)]
    Configuration(#[from] ClusterConfigError),
    /// State encoding or decoding failed.
    #[error("cluster catalog encoding failed: {0}")]
    Encoding(#[from] serde_json::Error),
    /// The durable catalog was written by an incompatible build.
    #[error(
        "cluster catalog format version {found} is newer than this build supports ({expected})"
    )]
    IncompatibleFormat {
        /// Version found on disk.
        found: u32,
        /// Version this build supports.
        expected: u32,
    },
    /// A durable operation failed.
    #[error("cluster catalog operation '{operation}' failed: {reason}")]
    Database {
        /// Stable operation name.
        operation: &'static str,
        /// Backend failure detail.
        reason: String,
    },
    /// A blocking catalog task could not finish.
    #[error("cluster catalog task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

fn backend<E: std::fmt::Display>(operation: &'static str, error: E) -> ClusterCatalogError {
    ClusterCatalogError::Database {
        operation,
        reason: error.to_string(),
    }
}

type CatalogResult<T> = Result<T, ClusterCatalogError>;

/// One raw key/value pair used by consensus snapshots.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CatalogEntry {
    /// Table the pair belongs to.
    pub table: String,
    /// Raw key bytes.
    pub key: Vec<u8>,
    /// Raw value bytes.
    pub value: Vec<u8>,
}

/// Creates every cluster table so that read transactions never fail on a fresh
/// database, and records the durable layout version.
pub fn initialize_tables(database: &Database) -> CatalogResult<()> {
    let write = database
        .begin_write()
        .map_err(|error| backend("begin cluster schema", error))?;
    for table in CLUSTER_TABLES {
        write
            .open_table(*table)
            .map_err(|error| backend("open cluster table", error))?;
    }
    {
        let mut schema = write
            .open_table(SCHEMA)
            .map_err(|error| backend("open cluster schema", error))?;
        let recorded = schema
            .get(SCHEMA_VERSION_KEY)
            .map_err(|error| backend("read cluster schema", error))?
            .map(|value| value.value().to_vec());
        match recorded {
            Some(encoded) => {
                let found = decode_u32(&encoded)?;
                if found > CLUSTER_FORMAT_VERSION {
                    return Err(ClusterCatalogError::IncompatibleFormat {
                        found,
                        expected: CLUSTER_FORMAT_VERSION,
                    });
                }
            }
            None => {
                schema
                    .insert(
                        SCHEMA_VERSION_KEY,
                        CLUSTER_FORMAT_VERSION.to_be_bytes().as_slice(),
                    )
                    .map_err(|error| backend("write cluster schema", error))?;
            }
        }
    }
    write
        .commit()
        .map_err(|error| backend("commit cluster schema", error))
}

/// Applies one command inside a caller-provided transaction.
///
/// Keeping the transaction external is what allows a consensus state machine to
/// persist the applied log position atomically with the state change, so a crash
/// can never apply an entry twice or lose it.
pub fn apply_command_tx(
    write: &WriteTransaction,
    command: ClusterCommand,
) -> CatalogResult<ClusterOutcome> {
    match command {
        ClusterCommand::InitializeCluster { identity, config } => {
            initialize_cluster(write, identity, *config)
        }
        ClusterCommand::UpdateConfig { config, at: _ } => {
            config.validate()?;
            put(write, CONFIG, SINGLETON, &*config)?;
            Ok(ClusterOutcome::Config(config))
        }
        ClusterCommand::RegisterNode { registration, at } => {
            register_node(write, *registration, at)
        }
        ClusterCommand::Heartbeat {
            node_id,
            capacity,
            activity,
            at,
        } => {
            let mut node = require_node(write, node_id)?;
            node.capacity = capacity;
            node.activity = activity;
            node.last_heartbeat_at = Some(at);
            put(write, NODES, node_id.as_uuid().as_bytes(), &node)?;
            Ok(ClusterOutcome::Node(Box::new(node)))
        }
        ClusterCommand::UpdateNodeDescriptor {
            node_id,
            rpc_address,
            s3_endpoint,
            versions,
            storage_class,
            failure_domain,
            started_at,
            at,
        } => {
            let mut node = require_node(write, node_id)?;
            node.rpc_address = rpc_address;
            node.s3_endpoint = s3_endpoint;
            node.protocol = versions.protocol;
            node.software_version = versions.software.clone();
            node.storage_format_version = versions.storage_format;
            node.storage_class = storage_class;
            node.failure_domain = failure_domain;
            node.started_at = started_at;
            node.last_heartbeat_at = Some(at);
            put(write, NODES, node_id.as_uuid().as_bytes(), &node)?;
            Ok(ClusterOutcome::Node(Box::new(node)))
        }
        ClusterCommand::SetNodeState {
            node_id,
            state,
            reason,
            at,
        } => {
            let mut node = require_node(write, node_id)?;
            let changed = node.transition(state, reason, at)?;
            if state == NodeState::Decommissioned {
                node.metadata_voter = false;
            }
            put(write, NODES, node_id.as_uuid().as_bytes(), &node)?;
            if changed {
                recount_durability(write)?;
            }
            Ok(ClusterOutcome::Node(Box::new(node)))
        }
        ClusterCommand::SetNodeMetadataVoter {
            node_id,
            voter,
            at: _,
        } => {
            let mut node = require_node(write, node_id)?;
            let changed = node.metadata_voter != voter;
            node.metadata_voter = voter;
            put(write, NODES, node_id.as_uuid().as_bytes(), &node)?;
            let _ = changed;
            Ok(ClusterOutcome::Node(Box::new(node)))
        }
        ClusterCommand::ForgetNode { node_id } => forget_node(write, node_id),
        ClusterCommand::PutPlacement { placement } => put_placement(write, *placement),
        ClusterCommand::UpsertReplica {
            object_id,
            replica,
            at,
        } => {
            let mut placement = require_placement(write, object_id)?;
            let before = physical_bytes(&placement);
            placement.upsert_replica(*replica, at);
            write_placement(write, &placement, before)?;
            Ok(ClusterOutcome::Placement(Box::new(placement)))
        }
        ClusterCommand::SetReplicaState {
            object_id,
            node_id,
            state,
            checksum,
            verified,
            at,
        } => {
            let mut placement = require_placement(write, object_id)?;
            let before = physical_bytes(&placement);
            {
                let replica = placement
                    .replicas
                    .iter_mut()
                    .find(|replica| replica.node_id == node_id)
                    .ok_or(ClusterCatalogError::ReplicaNotFound { object_id, node_id })?;
                let promoted =
                    replica.state != ReplicaState::Healthy && state == ReplicaState::Healthy;
                replica.state = state;
                if checksum.is_some() {
                    replica.checksum = checksum;
                }
                if verified {
                    replica.verified_at = Some(at);
                }
                if promoted {
                    replica.generation = replica.generation.saturating_add(1);
                }
            }
            placement.updated_at = at;
            write_placement(write, &placement, before)?;
            Ok(ClusterOutcome::Placement(Box::new(placement)))
        }
        ClusterCommand::RemoveReplica {
            object_id,
            node_id,
            at,
        } => {
            let mut placement = require_placement(write, object_id)?;
            let before = physical_bytes(&placement);
            let removed = placement.remove_replica(node_id, at);
            if removed {
                remove_node_replica(write, node_id, object_id)?;
            }
            write_placement(write, &placement, before)?;
            Ok(ClusterOutcome::Placement(Box::new(placement)))
        }
        ClusterCommand::SetDesiredReplicas {
            object_id,
            desired,
            at,
        } => {
            let mut placement = require_placement(write, object_id)?;
            let before = physical_bytes(&placement);
            placement.desired_replicas = desired;
            placement.updated_at = at;
            write_placement(write, &placement, before)?;
            Ok(ClusterOutcome::Placement(Box::new(placement)))
        }
        ClusterCommand::DeletePlacement { object_id, at } => delete_placement(write, object_id, at),
        ClusterCommand::AcknowledgeTombstone {
            object_id,
            node_id,
            at,
        } => {
            let mut tombstone: Tombstone = get(write, TOMBSTONES, object_id.as_uuid().as_bytes())?
                .ok_or(ClusterCatalogError::TombstoneNotFound(object_id))?;
            tombstone.acknowledge(node_id, at);
            remove_node_replica(write, node_id, object_id)?;
            put(
                write,
                TOMBSTONES,
                object_id.as_uuid().as_bytes(),
                &tombstone,
            )?;
            Ok(ClusterOutcome::Changed(tombstone.completed()))
        }
        ClusterCommand::PurgeTombstone { object_id } => {
            let removed = remove(write, TOMBSTONES, object_id.as_uuid().as_bytes())?;
            if removed {
                adjust_counter(write, TOMBSTONE_COUNT, -1)?;
            }
            Ok(ClusterOutcome::Changed(removed))
        }
        ClusterCommand::EnqueueTask { task } => enqueue_task(write, *task),
        ClusterCommand::ClaimTask {
            task_id,
            node_id,
            lease_seconds,
            at,
        } => {
            let mut task = require_task(write, task_id)?;
            if !matches!(task.state, ReplicaTaskState::Queued) {
                return Ok(ClusterOutcome::Changed(false));
            }
            remove_task_queue_entry(write, &task)?;
            task.claim(node_id, lease_seconds, at);
            insert_task_queue_entry(write, &task)?;
            put(write, TASKS, task_id.as_uuid().as_bytes(), &task)?;
            Ok(ClusterOutcome::Task(Box::new(task)))
        }
        ClusterCommand::CompleteTask { task_id, at } => {
            let mut task = require_task(write, task_id)?;
            remove_task_queue_entry(write, &task)?;
            let was_active = task.state.active();
            task.complete(at);
            put(write, TASKS, task_id.as_uuid().as_bytes(), &task)?;
            remove_task_object_entry(write, &task)?;
            if was_active {
                adjust_counter(write, ACTIVE_TASKS, -1)?;
            }
            Ok(ClusterOutcome::Task(Box::new(task)))
        }
        ClusterCommand::FailTask {
            task_id,
            reason,
            maximum_attempts,
            at,
        } => {
            let mut task = require_task(write, task_id)?;
            remove_task_queue_entry(write, &task)?;
            let was_active = task.state.active();
            task.fail(reason, maximum_attempts, at);
            if task.state.active() {
                insert_task_queue_entry(write, &task)?;
            } else {
                remove_task_object_entry(write, &task)?;
                if was_active {
                    adjust_counter(write, ACTIVE_TASKS, -1)?;
                }
                adjust_counter(write, PARKED_TASKS, 1)?;
            }
            put(write, TASKS, task_id.as_uuid().as_bytes(), &task)?;
            Ok(ClusterOutcome::Task(Box::new(task)))
        }
        ClusterCommand::RequeueTask {
            task_id,
            reason,
            at,
        } => {
            let mut task = require_task(write, task_id)?;
            remove_task_queue_entry(write, &task)?;
            let was_active = task.state.active();
            task.requeue(reason, at);
            insert_task_queue_entry(write, &task)?;
            put(write, TASKS, task_id.as_uuid().as_bytes(), &task)?;
            if !was_active {
                adjust_counter(write, ACTIVE_TASKS, 1)?;
                adjust_counter(write, PARKED_TASKS, -1)?;
            }
            Ok(ClusterOutcome::Task(Box::new(task)))
        }
        ClusterCommand::CancelTask {
            task_id,
            reason,
            at,
        } => {
            let mut task = require_task(write, task_id)?;
            remove_task_queue_entry(write, &task)?;
            let was_active = task.state.active();
            task.cancel(reason, at);
            put(write, TASKS, task_id.as_uuid().as_bytes(), &task)?;
            remove_task_object_entry(write, &task)?;
            if was_active {
                adjust_counter(write, ACTIVE_TASKS, -1)?;
            }
            Ok(ClusterOutcome::Task(Box::new(task)))
        }
        ClusterCommand::PurgeTask { task_id } => {
            let Some(task) = get::<ReplicaTask>(write, TASKS, task_id.as_uuid().as_bytes())? else {
                return Ok(ClusterOutcome::Changed(false));
            };
            if task.state.active() {
                return Ok(ClusterOutcome::Changed(false));
            }
            if matches!(task.state, ReplicaTaskState::Parked { .. }) {
                adjust_counter(write, PARKED_TASKS, -1)?;
            }
            remove(write, TASKS, task_id.as_uuid().as_bytes())?;
            Ok(ClusterOutcome::Changed(true))
        }
        ClusterCommand::StartOperation { operation } => {
            put(
                write,
                OPERATIONS,
                operation.id.as_uuid().as_bytes(),
                &*operation,
            )?;
            Ok(ClusterOutcome::Operation(operation))
        }
        ClusterCommand::UpdateOperation {
            operation_id,
            state,
            progress,
            message,
            at,
        } => {
            let mut operation: ClusterOperation =
                get(write, OPERATIONS, operation_id.as_uuid().as_bytes())?
                    .ok_or(ClusterCatalogError::OperationNotFound(operation_id))?;
            operation.state = state;
            operation.progress = progress;
            operation.updated_at = at;
            if message.is_some() {
                operation.message = message;
            }
            if !state.active() && operation.completed_at.is_none() {
                operation.completed_at = Some(at);
            }
            put(
                write,
                OPERATIONS,
                operation_id.as_uuid().as_bytes(),
                &operation,
            )?;
            Ok(ClusterOutcome::Operation(Box::new(operation)))
        }
        ClusterCommand::IssueJoinToken { token } => {
            put(write, JOIN_TOKENS, token.id.as_uuid().as_bytes(), &*token)?;
            Ok(ClusterOutcome::None)
        }
        ClusterCommand::ConsumeJoinToken { token_id, at: _ } => {
            let mut token: JoinToken = get(write, JOIN_TOKENS, token_id.as_uuid().as_bytes())?
                .ok_or(ClusterCatalogError::JoinTokenNotFound(token_id))?;
            token.consume();
            put(write, JOIN_TOKENS, token_id.as_uuid().as_bytes(), &token)?;
            Ok(ClusterOutcome::Changed(true))
        }
        ClusterCommand::RevokeJoinToken { token_id, at: _ } => {
            let mut token: JoinToken = get(write, JOIN_TOKENS, token_id.as_uuid().as_bytes())?
                .ok_or(ClusterCatalogError::JoinTokenNotFound(token_id))?;
            token.revoked = true;
            put(write, JOIN_TOKENS, token_id.as_uuid().as_bytes(), &token)?;
            Ok(ClusterOutcome::Changed(true))
        }
        ClusterCommand::PurgeJoinToken { token_id } => {
            let removed = remove(write, JOIN_TOKENS, token_id.as_uuid().as_bytes())?;
            Ok(ClusterOutcome::Changed(removed))
        }
        ClusterCommand::PutNodeCredential { credential } => {
            put(
                write,
                NODE_CREDENTIALS,
                credential.node_id.as_uuid().as_bytes(),
                &*credential,
            )?;
            Ok(ClusterOutcome::None)
        }
        ClusterCommand::SetNodeCredentialDisabled {
            node_id,
            disabled,
            at,
        } => {
            let mut credential: NodeCredential =
                get(write, NODE_CREDENTIALS, node_id.as_uuid().as_bytes())?
                    .ok_or(ClusterCatalogError::CredentialNotFound(node_id))?;
            credential.disabled = disabled;
            credential.rotated_at = Some(at);
            put(
                write,
                NODE_CREDENTIALS,
                node_id.as_uuid().as_bytes(),
                &credential,
            )?;
            Ok(ClusterOutcome::Changed(true))
        }
    }
}

fn initialize_cluster(
    write: &WriteTransaction,
    identity: ClusterIdentity,
    config: ClusterConfig,
) -> CatalogResult<ClusterOutcome> {
    config.validate()?;
    if let Some(existing) = get::<ClusterIdentity>(write, IDENTITY, SINGLETON)? {
        if existing.cluster_id == identity.cluster_id {
            return Ok(ClusterOutcome::Identity(Box::new(existing)));
        }
        return Err(ClusterCatalogError::AlreadyInitialized(existing.cluster_id));
    }
    put(write, IDENTITY, SINGLETON, &identity)?;
    put(write, CONFIG, SINGLETON, &config)?;
    set_counter(write, NEXT_MEMBER_ID, 1)?;
    Ok(ClusterOutcome::Identity(Box::new(identity)))
}

fn register_node(
    write: &WriteTransaction,
    registration: crate::topology::NodeRegistration,
    at: DateTime<Utc>,
) -> CatalogResult<ClusterOutcome> {
    let node_id = registration.node_id;
    if let Some(mut existing) = get::<NodeRecord>(write, NODES, node_id.as_uuid().as_bytes())? {
        // Re-registration after a restart keeps the assigned member identifier
        // and refreshes only what the node can legitimately re-declare.
        existing.protocol = registration.versions.protocol;
        existing.software_version = registration.versions.software.clone();
        existing.storage_format_version = registration.versions.storage_format;
        existing.rpc_address = registration.rpc_address;
        existing.s3_endpoint = registration.s3_endpoint;
        existing.storage_class = registration.storage_class;
        existing.failure_domain = registration.failure_domain;
        existing.capacity = registration.capacity;
        existing.started_at = registration.started_at;
        existing.last_heartbeat_at = Some(at);
        if existing.state == NodeState::Offline
            || existing.state == NodeState::Unreachable
            || existing.state == NodeState::Suspect
        {
            // A returning node must reconcile its replicas before it is trusted,
            // so it re-enters the cluster as Joining rather than Healthy.
            existing.transition(NodeState::Joining, Some("node restarted".into()), at)?;
        }
        let raft_id = existing.raft_id;
        put(write, NODES, node_id.as_uuid().as_bytes(), &existing)?;
        return Ok(ClusterOutcome::Registration {
            record: Box::new(existing),
            raft_id,
            created: false,
        });
    }
    let raft_id = next_member_id(write)?;
    let voters = count_voters(write)?;
    let config = require_config(write)?;
    let metadata_voter = voters < u32::from(config.metadata_voter_target);
    let record = NodeRecord::joining(registration, raft_id, metadata_voter, at);
    put(write, NODES, node_id.as_uuid().as_bytes(), &record)?;
    put_raw(
        write,
        NODE_BY_MEMBER,
        &raft_id.to_be_bytes(),
        node_id.as_uuid().as_bytes(),
    )?;
    Ok(ClusterOutcome::Registration {
        record: Box::new(record),
        raft_id,
        created: true,
    })
}

fn forget_node(write: &WriteTransaction, node_id: NodeId) -> CatalogResult<ClusterOutcome> {
    let Some(node) = get::<NodeRecord>(write, NODES, node_id.as_uuid().as_bytes())? else {
        return Ok(ClusterOutcome::Changed(false));
    };
    if node.state != NodeState::Decommissioned {
        return Err(ClusterCatalogError::Topology(
            TopologyError::InvalidStateTransition {
                from: node.state,
                to: NodeState::Decommissioned,
            },
        ));
    }
    if node_replica_count(write, node_id)? > 0 {
        return Err(ClusterCatalogError::Database {
            operation: "forget node",
            reason: "node still owns replica records".into(),
        });
    }
    remove(write, NODES, node_id.as_uuid().as_bytes())?;
    remove(write, NODE_BY_MEMBER, &node.raft_id.to_be_bytes())?;
    remove(write, NODE_CREDENTIALS, node_id.as_uuid().as_bytes())?;
    Ok(ClusterOutcome::Changed(true))
}

fn put_placement(
    write: &WriteTransaction,
    placement: PayloadPlacement,
) -> CatalogResult<ClusterOutcome> {
    let existing =
        get::<PayloadPlacement>(write, PLACEMENTS, placement.object_id.as_uuid().as_bytes())?;
    let before = existing.as_ref().map_or(0, physical_bytes);
    if existing.is_none() {
        adjust_counter(write, PLACEMENT_COUNT, 1)?;
        adjust_counter(write, LOGICAL_BYTES, i128::from(placement.size))?;
    } else if let Some(previous) = &existing {
        adjust_counter(
            write,
            LOGICAL_BYTES,
            i128::from(placement.size) - i128::from(previous.size),
        )?;
    }
    write_placement(write, &placement, before)?;
    Ok(ClusterOutcome::Placement(Box::new(placement)))
}

fn delete_placement(
    write: &WriteTransaction,
    object_id: ObjectId,
    at: DateTime<Utc>,
) -> CatalogResult<ClusterOutcome> {
    let Some(placement) =
        get::<PayloadPlacement>(write, PLACEMENTS, object_id.as_uuid().as_bytes())?
    else {
        // Deleting an unknown payload is a no-op, which keeps the operation
        // idempotent when a delete is retried after a leader change.
        return Ok(ClusterOutcome::Changed(false));
    };
    let holders: BTreeSet<NodeId> = placement.nodes();
    let tombstone = Tombstone::new(object_id, holders, at);
    let existing_tombstone = get::<Tombstone>(write, TOMBSTONES, object_id.as_uuid().as_bytes())?;
    put(
        write,
        TOMBSTONES,
        object_id.as_uuid().as_bytes(),
        &tombstone,
    )?;
    if existing_tombstone.is_none() {
        adjust_counter(write, TOMBSTONE_COUNT, 1)?;
    }
    adjust_counter(write, PLACEMENT_COUNT, -1)?;
    adjust_counter(write, LOGICAL_BYTES, -i128::from(placement.size))?;
    adjust_counter(
        write,
        PHYSICAL_BYTES,
        -i128::from(physical_bytes(&placement)),
    )?;
    remove(write, PLACEMENTS, object_id.as_uuid().as_bytes())?;
    for replica in &placement.replicas {
        // The node-replica index is kept so that a node returning from an outage
        // still learns it must delete these bytes.
        put_raw(
            write,
            NODE_REPLICAS,
            &node_replica_key(replica.node_id, object_id),
            &[replica_state_code(ReplicaState::Deleting)],
        )?;
    }
    Ok(ClusterOutcome::Changed(true))
}

fn enqueue_task(write: &WriteTransaction, task: ReplicaTask) -> CatalogResult<ClusterOutcome> {
    let dedupe_key = task_object_key(task.object_id, task.kind);
    if let Some(existing_id) = get_raw(write, TASK_BY_OBJECT, &dedupe_key)?
        && let Some(existing) = get::<ReplicaTask>(write, TASKS, &existing_id)?
        && existing.state.active()
    {
        // Repeating an identical request must not create duplicate work.
        return Ok(ClusterOutcome::Task(Box::new(existing)));
    }
    put(write, TASKS, task.id.as_uuid().as_bytes(), &task)?;
    put_raw(
        write,
        TASK_BY_OBJECT,
        &dedupe_key,
        task.id.as_uuid().as_bytes(),
    )?;
    insert_task_queue_entry(write, &task)?;
    adjust_counter(write, ACTIVE_TASKS, 1)?;
    Ok(ClusterOutcome::Task(Box::new(task)))
}

fn write_placement(
    write: &WriteTransaction,
    placement: &PayloadPlacement,
    physical_before: u64,
) -> CatalogResult<()> {
    let after = physical_bytes(placement);
    adjust_counter(
        write,
        PHYSICAL_BYTES,
        i128::from(after) - i128::from(physical_before),
    )?;
    put(
        write,
        PLACEMENTS,
        placement.object_id.as_uuid().as_bytes(),
        placement,
    )?;
    for replica in &placement.replicas {
        put_raw(
            write,
            NODE_REPLICAS,
            &node_replica_key(replica.node_id, placement.object_id),
            &[replica_state_code(replica.state)],
        )?;
    }
    Ok(())
}

fn physical_bytes(placement: &PayloadPlacement) -> u64 {
    placement
        .replicas
        .iter()
        .filter(|replica| {
            matches!(
                replica.state,
                ReplicaState::Healthy
                    | ReplicaState::Pending
                    | ReplicaState::Repairing
                    | ReplicaState::Stale
                    | ReplicaState::Corrupt
            )
        })
        .fold(0_u64, |total, replica| total.saturating_add(replica.size))
}

fn recount_durability(write: &WriteTransaction) -> CatalogResult<()> {
    // Node state changes can move many payloads in and out of the
    // under-replicated set at once, so the summary counters are recomputed.
    let identity = get::<ClusterIdentity>(write, IDENTITY, SINGLETON)?;
    if identity.is_none() {
        return Ok(());
    }
    let config = require_config(write)?;
    let nodes = read_nodes(write)?;
    let topology = ClusterTopology::new(
        identity
            .map(|identity| identity.cluster_id)
            .unwrap_or_default(),
        config,
        nodes,
    );
    let table = write
        .open_table(PLACEMENTS)
        .map_err(|error| backend("open placements", error))?;
    let mut under = 0_u64;
    let mut unavailable = 0_u64;
    for entry in table
        .iter()
        .map_err(|error| backend("scan placements", error))?
    {
        let (_, value) = entry.map_err(|error| backend("read placement", error))?;
        let placement: PayloadPlacement = serde_json::from_slice(value.value())?;
        let durability = placement.durability(&topology);
        if durability.under_replicated() {
            under = under.saturating_add(1);
        }
        if durability.unavailable() {
            unavailable = unavailable.saturating_add(1);
        }
    }
    drop(table);
    set_counter(write, UNDER_REPLICATED, under)?;
    set_counter(write, UNAVAILABLE_PAYLOADS, unavailable)?;
    Ok(())
}

fn insert_task_queue_entry(write: &WriteTransaction, task: &ReplicaTask) -> CatalogResult<()> {
    if !task.state.active() {
        return Ok(());
    }
    put_raw(
        write,
        TASK_QUEUE,
        &task_queue_key(task),
        task.id.as_uuid().as_bytes(),
    )
}

fn remove_task_queue_entry(write: &WriteTransaction, task: &ReplicaTask) -> CatalogResult<()> {
    remove(write, TASK_QUEUE, &task_queue_key(task)).map(|_| ())
}

fn remove_task_object_entry(write: &WriteTransaction, task: &ReplicaTask) -> CatalogResult<()> {
    let key = task_object_key(task.object_id, task.kind);
    if let Some(existing) = get_raw(write, TASK_BY_OBJECT, &key)?
        && existing.as_slice() == task.id.as_uuid().as_bytes()
    {
        remove(write, TASK_BY_OBJECT, &key)?;
    }
    Ok(())
}

fn remove_node_replica(
    write: &WriteTransaction,
    node_id: NodeId,
    object_id: ObjectId,
) -> CatalogResult<()> {
    remove(write, NODE_REPLICAS, &node_replica_key(node_id, object_id)).map(|_| ())
}

fn node_replica_count(write: &WriteTransaction, node_id: NodeId) -> CatalogResult<u64> {
    let table = write
        .open_table(NODE_REPLICAS)
        .map_err(|error| backend("open node replicas", error))?;
    let prefix = node_id.as_uuid().as_bytes().to_vec();
    let end = prefix_successor(&prefix);
    let mut count = 0_u64;
    for entry in table
        .range(prefix.as_slice()..end.as_slice())
        .map_err(|error| backend("range node replicas", error))?
    {
        entry.map_err(|error| backend("read node replica", error))?;
        count = count.saturating_add(1);
    }
    Ok(count)
}

fn task_queue_key(task: &ReplicaTask) -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + 8 + 16);
    key.push(task.priority.rank());
    key.extend_from_slice(&ordered_timestamp(task.created_at));
    key.extend_from_slice(task.id.as_uuid().as_bytes());
    key
}

fn task_object_key(object_id: ObjectId, kind: ReplicaTaskKind) -> Vec<u8> {
    let mut key = Vec::with_capacity(17);
    key.extend_from_slice(object_id.as_uuid().as_bytes());
    key.push(task_kind_code(kind));
    key
}

fn node_replica_key(node_id: NodeId, object_id: ObjectId) -> Vec<u8> {
    let mut key = Vec::with_capacity(32);
    key.extend_from_slice(node_id.as_uuid().as_bytes());
    key.extend_from_slice(object_id.as_uuid().as_bytes());
    key
}

const fn task_kind_code(kind: ReplicaTaskKind) -> u8 {
    match kind {
        ReplicaTaskKind::Repair => 0,
        ReplicaTaskKind::RepairCorrupt => 1,
        ReplicaTaskKind::Drain => 2,
        ReplicaTaskKind::Rebalance => 3,
        ReplicaTaskKind::RebalanceDomain => 4,
        ReplicaTaskKind::Delete => 5,
    }
}

const fn replica_state_code(state: ReplicaState) -> u8 {
    match state {
        ReplicaState::Pending => 0,
        ReplicaState::Healthy => 1,
        ReplicaState::Repairing => 2,
        ReplicaState::Stale => 3,
        ReplicaState::Missing => 4,
        ReplicaState::Deleting => 5,
        ReplicaState::Corrupt => 6,
    }
}

fn ordered_timestamp(value: DateTime<Utc>) -> [u8; 8] {
    // Flip the sign bit so that byte ordering matches numeric ordering.
    ((value.timestamp_millis() as u64) ^ (1_u64 << 63)).to_be_bytes()
}

fn prefix_successor(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last != u8::MAX {
            end.push(last + 1);
            return end;
        }
    }
    vec![u8::MAX; prefix.len() + 1]
}

fn put<T: Serialize>(
    write: &WriteTransaction,
    definition: TableDefinition<&[u8], &[u8]>,
    key: &[u8],
    value: &T,
) -> CatalogResult<()> {
    let encoded = serde_json::to_vec(value)?;
    put_raw(write, definition, key, &encoded)
}

fn put_raw(
    write: &WriteTransaction,
    definition: TableDefinition<&[u8], &[u8]>,
    key: &[u8],
    value: &[u8],
) -> CatalogResult<()> {
    let mut table = write
        .open_table(definition)
        .map_err(|error| backend("open cluster table", error))?;
    table
        .insert(key, value)
        .map_err(|error| backend("write cluster record", error))?;
    Ok(())
}

fn get<T: DeserializeOwned>(
    write: &WriteTransaction,
    definition: TableDefinition<&[u8], &[u8]>,
    key: &[u8],
) -> CatalogResult<Option<T>> {
    match get_raw(write, definition, key)? {
        Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        None => Ok(None),
    }
}

fn get_raw(
    write: &WriteTransaction,
    definition: TableDefinition<&[u8], &[u8]>,
    key: &[u8],
) -> CatalogResult<Option<Vec<u8>>> {
    let table = write
        .open_table(definition)
        .map_err(|error| backend("open cluster table", error))?;
    Ok(table
        .get(key)
        .map_err(|error| backend("read cluster record", error))?
        .map(|value| value.value().to_vec()))
}

fn remove(
    write: &WriteTransaction,
    definition: TableDefinition<&[u8], &[u8]>,
    key: &[u8],
) -> CatalogResult<bool> {
    let mut table = write
        .open_table(definition)
        .map_err(|error| backend("open cluster table", error))?;
    Ok(table
        .remove(key)
        .map_err(|error| backend("remove cluster record", error))?
        .is_some())
}

fn require_node(write: &WriteTransaction, node_id: NodeId) -> CatalogResult<NodeRecord> {
    get(write, NODES, node_id.as_uuid().as_bytes())?
        .ok_or(ClusterCatalogError::NodeNotFound(node_id))
}

fn require_placement(
    write: &WriteTransaction,
    object_id: ObjectId,
) -> CatalogResult<PayloadPlacement> {
    get(write, PLACEMENTS, object_id.as_uuid().as_bytes())?
        .ok_or(ClusterCatalogError::PlacementNotFound(object_id))
}

fn require_task(write: &WriteTransaction, task_id: ReplicaTaskId) -> CatalogResult<ReplicaTask> {
    get(write, TASKS, task_id.as_uuid().as_bytes())?
        .ok_or(ClusterCatalogError::TaskNotFound(task_id))
}

fn require_config(write: &WriteTransaction) -> CatalogResult<ClusterConfig> {
    get(write, CONFIG, SINGLETON)?.ok_or(ClusterCatalogError::NotInitialized)
}

fn read_nodes(write: &WriteTransaction) -> CatalogResult<Vec<NodeRecord>> {
    let table = write
        .open_table(NODES)
        .map_err(|error| backend("open nodes", error))?;
    let mut nodes = Vec::new();
    for entry in table.iter().map_err(|error| backend("scan nodes", error))? {
        let (_, value) = entry.map_err(|error| backend("read node", error))?;
        nodes.push(serde_json::from_slice(value.value())?);
    }
    Ok(nodes)
}

fn count_voters(write: &WriteTransaction) -> CatalogResult<u32> {
    Ok(u32::try_from(
        read_nodes(write)?
            .iter()
            .filter(|node| node.metadata_voter && !node.state.is_terminal())
            .count(),
    )
    .unwrap_or(u32::MAX))
}

fn next_member_id(write: &WriteTransaction) -> CatalogResult<RaftNodeId> {
    let current = read_counter(write, NEXT_MEMBER_ID)?.max(1);
    set_counter(write, NEXT_MEMBER_ID, current.saturating_add(1))?;
    Ok(current)
}

fn read_counter(write: &WriteTransaction, key: &[u8]) -> CatalogResult<u64> {
    match get_raw(write, COUNTERS, key)? {
        Some(bytes) => decode_u64(&bytes),
        None => Ok(0),
    }
}

fn set_counter(write: &WriteTransaction, key: &[u8], value: u64) -> CatalogResult<()> {
    put_raw(write, COUNTERS, key, value.to_be_bytes().as_slice())
}

fn adjust_counter(write: &WriteTransaction, key: &[u8], delta: i128) -> CatalogResult<()> {
    let current = i128::from(read_counter(write, key)?);
    let next = u64::try_from(current.saturating_add(delta).max(0)).unwrap_or(u64::MAX);
    set_counter(write, key, next)
}

fn decode_u64(bytes: &[u8]) -> CatalogResult<u64> {
    let array: [u8; 8] = bytes
        .try_into()
        .map_err(|_| ClusterCatalogError::Database {
            operation: "decode counter",
            reason: "counter value is not eight bytes".into(),
        })?;
    Ok(u64::from_be_bytes(array))
}

fn decode_u32(bytes: &[u8]) -> CatalogResult<u32> {
    let array: [u8; 4] = bytes
        .try_into()
        .map_err(|_| ClusterCatalogError::Database {
            operation: "decode version",
            reason: "version value is not four bytes".into(),
        })?;
    Ok(u32::from_be_bytes(array))
}

/// Aggregated cluster-wide storage accounting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClusterUsage {
    /// Payloads with placement metadata.
    pub payloads: u64,
    /// Logical bytes stored once, ignoring replication.
    pub logical_bytes: u64,
    /// Physical bytes across all replicas.
    pub physical_bytes: u64,
    /// Outstanding tombstones.
    pub tombstones: u64,
    /// Active replica movement tasks.
    pub active_tasks: u64,
    /// Tasks parked after exhausting their retries.
    pub parked_tasks: u64,
    /// Payloads below their desired replica count.
    pub under_replicated_payloads: u64,
    /// Payloads with no healthy replica.
    pub unavailable_payloads: u64,
}

/// Bounded page of replica movement tasks in priority order.
#[derive(Debug, Clone, Default)]
pub struct TaskPage {
    /// Tasks in priority order.
    pub tasks: Vec<ReplicaTask>,
    /// Whether more tasks matched than were returned.
    pub truncated: bool,
}

/// Bounded page of payload placements.
#[derive(Debug, Clone, Default)]
pub struct PlacementPage {
    /// Placements in payload-identifier order.
    pub placements: Vec<PayloadPlacement>,
    /// Continuation cursor.
    pub next_object_id: Option<ObjectId>,
}

/// Durable cluster catalog handle.
#[derive(Clone)]
pub struct ClusterCatalog {
    database: Arc<Database>,
}

impl ClusterCatalog {
    /// Opens a catalog in its own database file.
    pub async fn open(path: impl AsRef<Path>) -> CatalogResult<Self> {
        let path = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| backend("create directory", error))?;
            }
            let database =
                Database::create(path).map_err(|error| backend("open catalog", error))?;
            initialize_tables(&database)?;
            Ok(Self {
                database: Arc::new(database),
            })
        })
        .await?
    }

    /// Opens a catalog that shares a database with other OES state.
    ///
    /// Sharing one database is what lets a consensus state machine commit
    /// object metadata, cluster metadata, and the applied log position together.
    pub fn from_database(database: Arc<Database>) -> CatalogResult<Self> {
        initialize_tables(&database)?;
        Ok(Self { database })
    }

    /// Returns the shared database handle.
    #[must_use]
    pub fn database(&self) -> Arc<Database> {
        Arc::clone(&self.database)
    }

    /// Applies one command in its own transaction.
    ///
    /// Cluster mode routes commands through consensus instead; this entry point
    /// serves standalone deployments and tests.
    pub async fn apply(&self, command: ClusterCommand) -> CatalogResult<ClusterOutcome> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = database
                .begin_write()
                .map_err(|error| backend("begin cluster command", error))?;
            let outcome = apply_command_tx(&write, command)?;
            write
                .commit()
                .map_err(|error| backend("commit cluster command", error))?;
            Ok(outcome)
        })
        .await?
    }

    /// Returns the cluster identity, when the cluster has been initialized.
    pub async fn identity(&self) -> CatalogResult<Option<ClusterIdentity>> {
        self.read(|write| get(write, IDENTITY, SINGLETON)).await
    }

    /// Returns the cluster-wide configuration.
    pub async fn config(&self) -> CatalogResult<Option<ClusterConfig>> {
        self.read(|write| get(write, CONFIG, SINGLETON)).await
    }

    /// Returns one node record.
    pub async fn node(&self, node_id: NodeId) -> CatalogResult<Option<NodeRecord>> {
        self.read(move |write| get(write, NODES, node_id.as_uuid().as_bytes()))
            .await
    }

    /// Returns the node owning a consensus member identifier.
    pub async fn node_by_member(&self, raft_id: RaftNodeId) -> CatalogResult<Option<NodeRecord>> {
        self.read(move |write| {
            let Some(encoded) = get_raw(write, NODE_BY_MEMBER, &raft_id.to_be_bytes())? else {
                return Ok(None);
            };
            get(write, NODES, &encoded)
        })
        .await
    }

    /// Returns every node record ordered by node identifier.
    pub async fn nodes(&self) -> CatalogResult<Vec<NodeRecord>> {
        self.read(read_nodes).await.map(|mut nodes| {
            nodes.sort_by_key(|node| node.node_id);
            nodes
        })
    }

    /// Returns a topology view suitable for placement decisions.
    pub async fn topology(&self) -> CatalogResult<ClusterTopology> {
        let (identity, config, nodes) =
            tokio::try_join!(self.identity(), self.config(), self.nodes())?;
        let identity = identity.ok_or(ClusterCatalogError::NotInitialized)?;
        let config = config.ok_or(ClusterCatalogError::NotInitialized)?;
        Ok(ClusterTopology::new(identity.cluster_id, config, nodes))
    }

    /// Returns placement metadata for one payload.
    pub async fn placement(&self, object_id: ObjectId) -> CatalogResult<Option<PayloadPlacement>> {
        self.read(move |write| get(write, PLACEMENTS, object_id.as_uuid().as_bytes()))
            .await
    }

    /// Returns a bounded page of placements ordered by payload identifier.
    pub async fn list_placements(
        &self,
        after: Option<ObjectId>,
        limit: usize,
    ) -> CatalogResult<PlacementPage> {
        let limit = limit.clamp(1, 10_000);
        self.read(move |write| {
            let table = write
                .open_table(PLACEMENTS)
                .map_err(|error| backend("open placements", error))?;
            let start = after.map_or_else(Vec::new, |id| {
                let mut key = id.as_uuid().as_bytes().to_vec();
                key.push(0);
                key
            });
            let mut placements = Vec::new();
            for entry in table
                .range(start.as_slice()..)
                .map_err(|error| backend("range placements", error))?
                .take(limit + 1)
            {
                let (_, value) = entry.map_err(|error| backend("read placement", error))?;
                placements.push(serde_json::from_slice::<PayloadPlacement>(value.value())?);
            }
            let next_object_id = if placements.len() > limit {
                placements.pop();
                placements.last().map(|placement| placement.object_id)
            } else {
                None
            };
            Ok(PlacementPage {
                placements,
                next_object_id,
            })
        })
        .await
    }

    /// Returns the payload identifiers a node is recorded as holding.
    pub async fn node_replicas(
        &self,
        node_id: NodeId,
        after: Option<ObjectId>,
        limit: usize,
    ) -> CatalogResult<Vec<ObjectId>> {
        let limit = limit.clamp(1, 100_000);
        self.read(move |write| {
            let table = write
                .open_table(NODE_REPLICAS)
                .map_err(|error| backend("open node replicas", error))?;
            let prefix = node_id.as_uuid().as_bytes().to_vec();
            let mut start =
                after.map_or_else(|| prefix.clone(), |id| node_replica_key(node_id, id));
            if after.is_some() {
                start.push(0);
            }
            let end = prefix_successor(&prefix);
            let mut out = Vec::new();
            for entry in table
                .range(start.as_slice()..end.as_slice())
                .map_err(|error| backend("range node replicas", error))?
                .take(limit)
            {
                let (key, _) = entry.map_err(|error| backend("read node replica", error))?;
                let raw = key.value();
                if raw.len() != 32 {
                    continue;
                }
                let bytes: [u8; 16] =
                    raw[16..32]
                        .try_into()
                        .map_err(|_| ClusterCatalogError::Database {
                            operation: "decode node replica",
                            reason: "payload identifier is malformed".into(),
                        })?;
                out.push(ObjectId::from_uuid(uuid::Uuid::from_bytes(bytes)));
            }
            Ok(out)
        })
        .await
    }

    /// Returns the number of replica records a node holds.
    pub async fn node_replica_count(&self, node_id: NodeId) -> CatalogResult<u64> {
        self.read(move |write| node_replica_count(write, node_id))
            .await
    }

    /// Returns the tombstone for a payload, if one exists.
    pub async fn tombstone(&self, object_id: ObjectId) -> CatalogResult<Option<Tombstone>> {
        self.read(move |write| get(write, TOMBSTONES, object_id.as_uuid().as_bytes()))
            .await
    }

    /// Returns tombstones that still have outstanding nodes.
    pub async fn pending_tombstones(&self, limit: usize) -> CatalogResult<Vec<Tombstone>> {
        let limit = limit.clamp(1, 10_000);
        self.read(move |write| {
            let table = write
                .open_table(TOMBSTONES)
                .map_err(|error| backend("open tombstones", error))?;
            let mut out = Vec::new();
            for entry in table
                .iter()
                .map_err(|error| backend("scan tombstones", error))?
            {
                let (_, value) = entry.map_err(|error| backend("read tombstone", error))?;
                let tombstone: Tombstone = serde_json::from_slice(value.value())?;
                if !tombstone.completed() {
                    out.push(tombstone);
                }
                if out.len() >= limit {
                    break;
                }
            }
            Ok(out)
        })
        .await
    }

    /// Returns tombstones that may be purged under the retention policy.
    pub async fn purgeable_tombstones(
        &self,
        retention_hours: u32,
        now: DateTime<Utc>,
        limit: usize,
    ) -> CatalogResult<Vec<ObjectId>> {
        let limit = limit.clamp(1, 10_000);
        self.read(move |write| {
            let table = write
                .open_table(TOMBSTONES)
                .map_err(|error| backend("open tombstones", error))?;
            let mut out = Vec::new();
            for entry in table
                .iter()
                .map_err(|error| backend("scan tombstones", error))?
            {
                let (_, value) = entry.map_err(|error| backend("read tombstone", error))?;
                let tombstone: Tombstone = serde_json::from_slice(value.value())?;
                if tombstone.purgeable(retention_hours, now) {
                    out.push(tombstone.object_id);
                }
                if out.len() >= limit {
                    break;
                }
            }
            Ok(out)
        })
        .await
    }

    /// Returns one task by identifier.
    pub async fn task(&self, task_id: ReplicaTaskId) -> CatalogResult<Option<ReplicaTask>> {
        self.read(move |write| get(write, TASKS, task_id.as_uuid().as_bytes()))
            .await
    }

    /// Returns active tasks in priority order, most urgent first.
    pub async fn queued_tasks(&self, limit: usize) -> CatalogResult<TaskPage> {
        let limit = limit.clamp(1, 10_000);
        self.read(move |write| {
            let queue = write
                .open_table(TASK_QUEUE)
                .map_err(|error| backend("open task queue", error))?;
            let mut tasks = Vec::new();
            let mut truncated = false;
            for entry in queue
                .iter()
                .map_err(|error| backend("scan task queue", error))?
            {
                let (_, value) = entry.map_err(|error| backend("read task queue", error))?;
                if tasks.len() >= limit {
                    truncated = true;
                    break;
                }
                if let Some(task) = get::<ReplicaTask>(write, TASKS, value.value())? {
                    tasks.push(task);
                }
            }
            Ok(TaskPage { tasks, truncated })
        })
        .await
    }

    /// Returns one long-running operation.
    pub async fn operation(
        &self,
        operation_id: ClusterOperationId,
    ) -> CatalogResult<Option<ClusterOperation>> {
        self.read(move |write| get(write, OPERATIONS, operation_id.as_uuid().as_bytes()))
            .await
    }

    /// Returns every recorded operation, newest first.
    pub async fn operations(&self, limit: usize) -> CatalogResult<Vec<ClusterOperation>> {
        let limit = limit.clamp(1, 1_000);
        self.read(move |write| {
            let table = write
                .open_table(OPERATIONS)
                .map_err(|error| backend("open operations", error))?;
            let mut out = Vec::new();
            for entry in table
                .iter()
                .map_err(|error| backend("scan operations", error))?
            {
                let (_, value) = entry.map_err(|error| backend("read operation", error))?;
                out.push(serde_json::from_slice::<ClusterOperation>(value.value())?);
            }
            out.sort_by_key(|operation| std::cmp::Reverse(operation.started_at));
            out.truncate(limit);
            Ok(out)
        })
        .await
    }

    /// Returns a join token record.
    pub async fn join_token(&self, token_id: JoinTokenId) -> CatalogResult<Option<JoinToken>> {
        self.read(move |write| get(write, JOIN_TOKENS, token_id.as_uuid().as_bytes()))
            .await
    }

    /// Returns a node credential record.
    pub async fn node_credential(&self, node_id: NodeId) -> CatalogResult<Option<NodeCredential>> {
        self.read(move |write| get(write, NODE_CREDENTIALS, node_id.as_uuid().as_bytes()))
            .await
    }

    /// Returns the node credential registered under a credential identifier.
    pub async fn node_credential_by_id(
        &self,
        credential_id: NodeCredentialId,
    ) -> CatalogResult<Option<NodeCredential>> {
        self.read(move |write| {
            let table = write
                .open_table(NODE_CREDENTIALS)
                .map_err(|error| backend("open node credentials", error))?;
            for entry in table
                .iter()
                .map_err(|error| backend("scan node credentials", error))?
            {
                let (_, value) = entry.map_err(|error| backend("read node credential", error))?;
                let credential: NodeCredential = serde_json::from_slice(value.value())?;
                if credential.id == credential_id {
                    return Ok(Some(credential));
                }
            }
            Ok(None)
        })
        .await
    }

    /// Returns aggregated cluster-wide accounting.
    pub async fn usage(&self) -> CatalogResult<ClusterUsage> {
        self.read(|write| {
            Ok(ClusterUsage {
                payloads: read_counter(write, PLACEMENT_COUNT)?,
                logical_bytes: read_counter(write, LOGICAL_BYTES)?,
                physical_bytes: read_counter(write, PHYSICAL_BYTES)?,
                tombstones: read_counter(write, TOMBSTONE_COUNT)?,
                active_tasks: read_counter(write, ACTIVE_TASKS)?,
                parked_tasks: read_counter(write, PARKED_TASKS)?,
                under_replicated_payloads: read_counter(write, UNDER_REPLICATED)?,
                unavailable_payloads: read_counter(write, UNAVAILABLE_PAYLOADS)?,
            })
        })
        .await
    }

    /// Recomputes the summary durability counters from placement records.
    pub async fn refresh_durability_counters(&self) -> CatalogResult<()> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = database
                .begin_write()
                .map_err(|error| backend("begin durability recount", error))?;
            recount_durability(&write)?;
            write
                .commit()
                .map_err(|error| backend("commit durability recount", error))
        })
        .await?
    }

    /// Verifies that the catalog is writable.
    pub async fn check_ready(&self) -> CatalogResult<()> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = database
                .begin_write()
                .map_err(|error| backend("cluster readiness", error))?;
            {
                write
                    .open_table(NODES)
                    .map_err(|error| backend("cluster readiness table", error))?;
            }
            write
                .commit()
                .map_err(|error| backend("commit cluster readiness", error))
        })
        .await?
    }

    async fn read<T, F>(&self, operation: F) -> CatalogResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&WriteTransaction) -> CatalogResult<T> + Send + 'static,
    {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            // A write transaction is used for reads so that helper functions can
            // be shared with the command path without duplicating them.
            let write = database
                .begin_write()
                .map_err(|error| backend("begin cluster read", error))?;
            let value = operation(&write)?;
            write
                .commit()
                .map_err(|error| backend("commit cluster read", error))?;
            Ok(value)
        })
        .await?
    }
}

/// Exports every cluster table for a consensus snapshot.
///
/// A read transaction is used so that a snapshot is a consistent point-in-time
/// view without blocking concurrent command application.
pub fn export_tx(write: &redb::ReadTransaction) -> CatalogResult<Vec<CatalogEntry>> {
    let mut entries = Vec::new();
    for definition in CLUSTER_TABLES {
        let table = write
            .open_table(*definition)
            .map_err(|error| backend("open cluster table", error))?;
        if table
            .is_empty()
            .map_err(|error| backend("inspect cluster table", error))?
        {
            continue;
        }
        for entry in table
            .iter()
            .map_err(|error| backend("scan cluster table", error))?
        {
            let (key, value) = entry.map_err(|error| backend("read cluster record", error))?;
            entries.push(CatalogEntry {
                table: definition.name().to_owned(),
                key: key.value().to_vec(),
                value: value.value().to_vec(),
            });
        }
    }
    Ok(entries)
}

/// Replaces every cluster table from a consensus snapshot.
pub fn import_tx(write: &WriteTransaction, entries: &[CatalogEntry]) -> CatalogResult<()> {
    let by_name: BTreeMap<&str, TableDefinition<&[u8], &[u8]>> = CLUSTER_TABLES
        .iter()
        .map(|definition| (definition.name(), *definition))
        .collect();
    for definition in CLUSTER_TABLES {
        let mut table = write
            .open_table(*definition)
            .map_err(|error| backend("open cluster table", error))?;
        table
            .retain(|_, _| false)
            .map_err(|error| backend("clear cluster table", error))?;
    }
    for entry in entries {
        let Some(definition) = by_name.get(entry.table.as_str()) else {
            return Err(ClusterCatalogError::Database {
                operation: "import cluster snapshot",
                reason: format!("snapshot references unknown table '{}'", entry.table),
            });
        };
        put_raw(write, *definition, &entry.key, &entry.value)?;
    }
    Ok(())
}

/// Returns how long a node has been silent, for failure detection.
#[must_use]
pub fn silence(node: &NodeRecord, now: DateTime<Utc>) -> TimeDelta {
    now.signed_duration_since(node.last_heartbeat_at.unwrap_or(node.joined_at))
}

#[cfg(test)]
mod tests {
    use oes_core::Checksum;

    use super::*;
    use crate::{
        replica::Replica,
        tasks::{ReplicaTaskKind, ReplicaTaskPriority},
        topology::{FailureDomain, NodeCapacity, NodeRegistration, StorageClass},
        version::NodeVersions,
    };

    async fn open_catalog() -> (tempfile::TempDir, ClusterCatalog) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let catalog = ClusterCatalog::open(directory.path().join("cluster.redb"))
            .await
            .expect("open catalog");
        (directory, catalog)
    }

    fn identity() -> ClusterIdentity {
        ClusterIdentity {
            cluster_id: ClusterId::new(),
            cluster_format_version: CLUSTER_FORMAT_VERSION,
            created_at: Utc::now(),
        }
    }

    fn registration() -> NodeRegistration {
        NodeRegistration {
            node_id: NodeId::new(),
            versions: NodeVersions::current("test"),
            rpc_address: "10.0.0.1:7603".into(),
            s3_endpoint: Some("http://10.0.0.1:7600".into()),
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

    async fn initialized() -> (tempfile::TempDir, ClusterCatalog) {
        let (directory, catalog) = open_catalog().await;
        catalog
            .apply(ClusterCommand::InitializeCluster {
                identity: identity(),
                config: Box::new(ClusterConfig::default()),
            })
            .await
            .expect("initialize cluster");
        (directory, catalog)
    }

    #[tokio::test]
    async fn initialization_is_idempotent_and_refuses_a_foreign_cluster() {
        let (_directory, catalog) = open_catalog().await;
        let first = identity();
        catalog
            .apply(ClusterCommand::InitializeCluster {
                identity: first.clone(),
                config: Box::new(ClusterConfig::default()),
            })
            .await
            .expect("initialize");
        catalog
            .apply(ClusterCommand::InitializeCluster {
                identity: first.clone(),
                config: Box::new(ClusterConfig::default()),
            })
            .await
            .expect("re-initialization with the same identity is a no-op");
        let error = catalog
            .apply(ClusterCommand::InitializeCluster {
                identity: identity(),
                config: Box::new(ClusterConfig::default()),
            })
            .await
            .expect_err("a different cluster identity must be refused");
        assert!(matches!(error, ClusterCatalogError::AlreadyInitialized(_)));
    }

    #[tokio::test]
    async fn registration_assigns_stable_member_identifiers() {
        let (_directory, catalog) = initialized().await;
        let first = registration();
        let node_id = first.node_id;
        let outcome = catalog
            .apply(ClusterCommand::RegisterNode {
                registration: Box::new(first.clone()),
                at: Utc::now(),
            })
            .await
            .expect("register");
        let ClusterOutcome::Registration {
            raft_id, created, ..
        } = outcome
        else {
            panic!("registration must return a member identifier");
        };
        assert_eq!(raft_id, 1);
        assert!(created);

        let second = catalog
            .apply(ClusterCommand::RegisterNode {
                registration: Box::new(first),
                at: Utc::now(),
            })
            .await
            .expect("re-register");
        let ClusterOutcome::Registration {
            raft_id: again,
            created: created_again,
            ..
        } = second
        else {
            panic!("re-registration must return a member identifier");
        };
        assert_eq!(again, 1, "restart must not change the member identifier");
        assert!(!created_again);

        let other = catalog
            .apply(ClusterCommand::RegisterNode {
                registration: Box::new(registration()),
                at: Utc::now(),
            })
            .await
            .expect("register second node");
        let ClusterOutcome::Registration { raft_id, .. } = other else {
            panic!("registration must return a member identifier");
        };
        assert_eq!(raft_id, 2);
        assert!(catalog.node(node_id).await.expect("read").is_some());
        assert!(
            catalog
                .node_by_member(1)
                .await
                .expect("read")
                .is_some_and(|node| node.node_id == node_id)
        );
    }

    #[tokio::test]
    async fn only_the_configured_number_of_nodes_become_voters() {
        let (_directory, catalog) = initialized().await;
        let mut voters = 0;
        for _ in 0..5 {
            let outcome = catalog
                .apply(ClusterCommand::RegisterNode {
                    registration: Box::new(registration()),
                    at: Utc::now(),
                })
                .await
                .expect("register");
            if outcome.node().is_some_and(|node| node.metadata_voter) {
                voters += 1;
            }
        }
        assert_eq!(voters, 3, "voter count must follow metadata_voter_target");
    }

    #[tokio::test]
    async fn deleting_a_placement_creates_a_tombstone_for_every_holder() {
        let (_directory, catalog) = initialized().await;
        let first = NodeId::new();
        let second = NodeId::new();
        let object_id = ObjectId::new();
        let now = Utc::now();
        let placement = PayloadPlacement::new(
            object_id,
            100,
            Checksum::sha256([1_u8; 32]),
            2,
            StorageClass::default(),
            vec![
                Replica::healthy(first, 100, Checksum::sha256([1_u8; 32]), now),
                Replica::healthy(second, 100, Checksum::sha256([1_u8; 32]), now),
            ],
            now,
        );
        catalog
            .apply(ClusterCommand::PutPlacement {
                placement: Box::new(placement),
            })
            .await
            .expect("commit placement");
        let usage = catalog.usage().await.expect("usage");
        assert_eq!(usage.payloads, 1);
        assert_eq!(usage.logical_bytes, 100);
        assert_eq!(usage.physical_bytes, 200);

        catalog
            .apply(ClusterCommand::DeletePlacement { object_id, at: now })
            .await
            .expect("delete placement");
        let tombstone = catalog
            .tombstone(object_id)
            .await
            .expect("read tombstone")
            .expect("tombstone must exist");
        assert_eq!(tombstone.pending_nodes.len(), 2);
        assert!(!tombstone.completed());
        let usage = catalog.usage().await.expect("usage");
        assert_eq!(usage.payloads, 0);
        assert_eq!(usage.physical_bytes, 0);
        assert_eq!(usage.tombstones, 1);

        catalog
            .apply(ClusterCommand::AcknowledgeTombstone {
                object_id,
                node_id: first,
                at: now,
            })
            .await
            .expect("acknowledge");
        catalog
            .apply(ClusterCommand::AcknowledgeTombstone {
                object_id,
                node_id: second,
                at: now,
            })
            .await
            .expect("acknowledge");
        let tombstone = catalog
            .tombstone(object_id)
            .await
            .expect("read tombstone")
            .expect("tombstone must exist");
        assert!(tombstone.completed());
    }

    #[tokio::test]
    async fn identical_task_requests_do_not_duplicate_work() {
        let (_directory, catalog) = initialized().await;
        let object_id = ObjectId::new();
        let now = Utc::now();
        let first = catalog
            .apply(ClusterCommand::EnqueueTask {
                task: Box::new(ReplicaTask::queued(
                    object_id,
                    ReplicaTaskKind::Repair,
                    ReplicaTaskPriority::High,
                    10,
                    now,
                )),
            })
            .await
            .expect("enqueue")
            .task()
            .expect("task");
        let second = catalog
            .apply(ClusterCommand::EnqueueTask {
                task: Box::new(ReplicaTask::queued(
                    object_id,
                    ReplicaTaskKind::Repair,
                    ReplicaTaskPriority::High,
                    10,
                    now,
                )),
            })
            .await
            .expect("enqueue")
            .task()
            .expect("task");
        assert_eq!(
            first.id, second.id,
            "duplicate repair requests must collapse"
        );
        let usage = catalog.usage().await.expect("usage");
        assert_eq!(usage.active_tasks, 1);
    }

    #[tokio::test]
    async fn queued_tasks_are_returned_in_risk_order() {
        let (_directory, catalog) = initialized().await;
        let now = Utc::now();
        for (kind, priority) in [
            (ReplicaTaskKind::Rebalance, ReplicaTaskPriority::Low),
            (ReplicaTaskKind::Repair, ReplicaTaskPriority::Unavailable),
            (ReplicaTaskKind::Drain, ReplicaTaskPriority::Normal),
        ] {
            catalog
                .apply(ClusterCommand::EnqueueTask {
                    task: Box::new(ReplicaTask::queued(
                        ObjectId::new(),
                        kind,
                        priority,
                        10,
                        now,
                    )),
                })
                .await
                .expect("enqueue");
        }
        let page = catalog.queued_tasks(10).await.expect("queued tasks");
        let priorities: Vec<_> = page.tasks.iter().map(|task| task.priority).collect();
        assert_eq!(
            priorities,
            vec![
                ReplicaTaskPriority::Unavailable,
                ReplicaTaskPriority::Normal,
                ReplicaTaskPriority::Low
            ]
        );
    }

    #[tokio::test]
    async fn snapshot_export_and_import_round_trip() {
        let (_directory, catalog) = initialized().await;
        catalog
            .apply(ClusterCommand::RegisterNode {
                registration: Box::new(registration()),
                at: Utc::now(),
            })
            .await
            .expect("register");
        let database = catalog.database();
        let entries = tokio::task::spawn_blocking({
            let database = Arc::clone(&database);
            move || {
                let read = database.begin_read().expect("begin");
                export_tx(&read).expect("export")
            }
        })
        .await
        .expect("join");
        assert!(!entries.is_empty());

        let (_other_directory, other) = open_catalog().await;
        let other_database = other.database();
        tokio::task::spawn_blocking(move || {
            let write = other_database.begin_write().expect("begin");
            import_tx(&write, &entries).expect("import");
            write.commit().expect("commit");
        })
        .await
        .expect("join");
        let restored = other.nodes().await.expect("nodes");
        assert_eq!(restored.len(), 1);
        assert!(other.identity().await.expect("identity").is_some());
    }
}
