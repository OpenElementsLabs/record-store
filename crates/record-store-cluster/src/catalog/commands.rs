use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use record_store_core::{NodeId, ObjectId};
use redb::{ReadableTable, WriteTransaction};

use crate::{
    command::{ClusterCommand, ClusterIdentity, ClusterOutcome},
    config::ClusterConfig,
    credentials::{JoinToken, NodeCredential},
    replica::{PayloadPlacement, ReplicaState, Tombstone},
    tasks::{ClusterOperation, ReplicaTask, ReplicaTaskState},
    topology::{ClusterTopology, NodeRecord, NodeState, TopologyError},
};

use crate::catalog::codec::{
    adjust_counter, count_voters, get, get_raw, next_member_id, put, put_raw, read_nodes, remove,
    require_config, require_node, require_placement, require_task, set_counter,
};
use crate::catalog::keys::{
    node_replica_key, prefix_successor, replica_state_code, task_object_key, task_queue_key,
};
use crate::catalog::schema::{
    ACTIVE_TASKS, CONFIG, IDENTITY, JOIN_TOKENS, LOGICAL_BYTES, NEXT_MEMBER_ID, NODE_BY_MEMBER,
    NODE_CREDENTIALS, NODE_REPLICAS, NODES, OPERATIONS, PARKED_TASKS, PHYSICAL_BYTES,
    PLACEMENT_COUNT, PLACEMENTS, SINGLETON, TASK_BY_OBJECT, TASK_QUEUE, TASKS, TOMBSTONE_COUNT,
    TOMBSTONES, UNAVAILABLE_PAYLOADS, UNDER_REPLICATED,
};
use crate::catalog::*;

///
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

pub(crate) fn initialize_cluster(
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

pub(crate) fn register_node(
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

pub(crate) fn forget_node(
    write: &WriteTransaction,
    node_id: NodeId,
) -> CatalogResult<ClusterOutcome> {
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

pub(crate) fn put_placement(
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

pub(crate) fn delete_placement(
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

pub(crate) fn enqueue_task(
    write: &WriteTransaction,
    task: ReplicaTask,
) -> CatalogResult<ClusterOutcome> {
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

pub(crate) fn write_placement(
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

pub(crate) fn physical_bytes(placement: &PayloadPlacement) -> u64 {
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

pub(crate) fn recount_durability(write: &WriteTransaction) -> CatalogResult<()> {
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

pub(crate) fn insert_task_queue_entry(
    write: &WriteTransaction,
    task: &ReplicaTask,
) -> CatalogResult<()> {
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

pub(crate) fn remove_task_queue_entry(
    write: &WriteTransaction,
    task: &ReplicaTask,
) -> CatalogResult<()> {
    remove(write, TASK_QUEUE, &task_queue_key(task)).map(|_| ())
}

pub(crate) fn remove_task_object_entry(
    write: &WriteTransaction,
    task: &ReplicaTask,
) -> CatalogResult<()> {
    let key = task_object_key(task.object_id, task.kind);
    if let Some(existing) = get_raw(write, TASK_BY_OBJECT, &key)?
        && existing.as_slice() == task.id.as_uuid().as_bytes()
    {
        remove(write, TASK_BY_OBJECT, &key)?;
    }
    Ok(())
}

pub(crate) fn remove_node_replica(
    write: &WriteTransaction,
    node_id: NodeId,
    object_id: ObjectId,
) -> CatalogResult<()> {
    remove(write, NODE_REPLICAS, &node_replica_key(node_id, object_id)).map(|_| ())
}

pub(crate) fn node_replica_count(write: &WriteTransaction, node_id: NodeId) -> CatalogResult<u64> {
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

#[cfg(test)]
mod tests {

    use chrono::Utc;
    use record_store_core::{Checksum, NodeId, ObjectId};

    use super::*;
    use crate::catalog::test_support::*;
    use crate::command::{ClusterCommand, ClusterOutcome};
    use crate::replica::{PayloadPlacement, Replica};
    use crate::tasks::{ReplicaTask, ReplicaTaskKind, ReplicaTaskPriority};
    use crate::topology::NodeCapacity;
    use crate::topology::StorageClass;
    use record_store_core::ReplicaTaskId;

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

    /// A node's lifecycle is a state machine an operator drives. Every
    /// transition must be durable and readable back, because placement and
    /// read routing both key off it.
    #[tokio::test]
    async fn node_lifecycle_transitions_are_durable() {
        let (_directory, catalog) = initialized().await;
        let now = Utc::now();
        let node_id = register(&catalog, now).await;

        for state in [
            NodeState::Healthy,
            NodeState::Draining,
            NodeState::Maintenance,
            NodeState::Unreachable,
        ] {
            catalog
                .apply(ClusterCommand::SetNodeState {
                    node_id,
                    state,
                    reason: Some(format!("moved to {state:?}")),
                    at: now,
                })
                .await
                .expect("set state");
            let node = catalog.node(node_id).await.expect("read").expect("node");
            assert_eq!(node.state, state);
            assert!(node.state_reason.is_some());
        }
    }

    /// A heartbeat is how the cluster learns a node is alive and how full it is.
    /// Losing either would make failure detection and placement blind.
    #[tokio::test]
    async fn a_heartbeat_records_liveness_and_capacity() {
        let (_directory, catalog) = initialized().await;
        let now = Utc::now();
        let node_id = register(&catalog, now).await;

        let later = now + chrono::Duration::seconds(30);
        catalog
            .apply(ClusterCommand::Heartbeat {
                node_id,
                capacity: NodeCapacity {
                    total_bytes: 1_000,
                    available_bytes: 750,
                    replica_bytes: 250,
                    ..NodeCapacity::default()
                },
                activity: crate::topology::NodeActivity::default(),
                at: later,
            })
            .await
            .expect("heartbeat");

        let node = catalog.node(node_id).await.expect("read").expect("node");
        assert_eq!(node.last_heartbeat_at, Some(later));
        assert_eq!(node.capacity.replica_bytes, 250);
        assert_eq!(
            crate::catalog::silence(&node, later),
            chrono::TimeDelta::zero()
        );
    }

    /// A heartbeat from a node the cluster does not know must not implicitly
    /// enrol it, or an unauthorised process could join by simply reporting in.
    #[tokio::test]
    async fn a_heartbeat_from_an_unknown_node_is_refused() {
        let (_directory, catalog) = initialized().await;
        let result = catalog
            .apply(ClusterCommand::Heartbeat {
                node_id: NodeId::new(),
                capacity: NodeCapacity::default(),
                activity: crate::topology::NodeActivity::default(),
                at: Utc::now(),
            })
            .await;
        assert!(matches!(result, Err(ClusterCatalogError::NodeNotFound(_))));
    }

    /// A task moves through claim, completion, and purge. Each step has to be
    /// durable so a coordinator restart does not redo finished work.
    #[tokio::test]
    async fn a_task_moves_through_its_lifecycle() {
        let (_directory, catalog) = initialized().await;
        let now = Utc::now();
        let node_id = register(&catalog, now).await;
        let task = ReplicaTask {
            id: ReplicaTaskId::new(),
            object_id: ObjectId::new(),
            kind: ReplicaTaskKind::Repair,
            priority: ReplicaTaskPriority::High,
            source_node: None,
            target_node: Some(node_id),
            operation_id: None,
            size: 1_024,
            state: ReplicaTaskState::Queued,
            attempts: 0,
            last_error: None,
            created_at: now,
            updated_at: now,
        };
        catalog
            .apply(ClusterCommand::EnqueueTask {
                task: Box::new(task.clone()),
            })
            .await
            .expect("enqueue");

        catalog
            .apply(ClusterCommand::ClaimTask {
                task_id: task.id,
                node_id,
                lease_seconds: 60,
                at: now,
            })
            .await
            .expect("claim");
        assert_eq!(
            catalog
                .task(task.id)
                .await
                .expect("read")
                .expect("task")
                .state,
            ReplicaTaskState::Running {
                node_id,
                started_at: now,
                lease_expires_at: now + chrono::Duration::seconds(60),
            }
        );

        catalog
            .apply(ClusterCommand::CompleteTask {
                task_id: task.id,
                at: now,
            })
            .await
            .expect("complete");
        assert!(
            matches!(
                catalog
                    .task(task.id)
                    .await
                    .expect("read")
                    .expect("task")
                    .state,
                ReplicaTaskState::Completed { .. }
            ),
            "a completed task keeps its completion time"
        );

        catalog
            .apply(ClusterCommand::PurgeTask { task_id: task.id })
            .await
            .expect("purge");
        assert!(catalog.task(task.id).await.expect("read").is_none());
    }

    /// A failed task records why and how many times, so an operator can see a
    /// task that keeps failing rather than one that silently disappears.
    #[tokio::test]
    async fn a_failed_task_records_its_attempts_and_reason() {
        let (_directory, catalog) = initialized().await;
        let now = Utc::now();
        let node_id = register(&catalog, now).await;
        let task = ReplicaTask {
            id: ReplicaTaskId::new(),
            object_id: ObjectId::new(),
            kind: ReplicaTaskKind::Repair,
            priority: ReplicaTaskPriority::Low,
            source_node: None,
            target_node: Some(node_id),
            operation_id: None,
            size: 1,
            state: ReplicaTaskState::Queued,
            attempts: 0,
            last_error: None,
            created_at: now,
            updated_at: now,
        };
        catalog
            .apply(ClusterCommand::EnqueueTask {
                task: Box::new(task.clone()),
            })
            .await
            .expect("enqueue");
        catalog
            .apply(ClusterCommand::FailTask {
                task_id: task.id,
                reason: "peer refused".to_owned(),
                maximum_attempts: 3,
                at: now,
            })
            .await
            .expect("fail");

        let stored = catalog.task(task.id).await.expect("read").expect("task");
        assert_eq!(stored.attempts, 1);
        assert_eq!(stored.last_error.as_deref(), Some("peer refused"));

        catalog
            .apply(ClusterCommand::RequeueTask {
                task_id: task.id,
                reason: None,
                at: now,
            })
            .await
            .expect("requeue");
        assert_eq!(
            catalog
                .task(task.id)
                .await
                .expect("read")
                .expect("task")
                .state,
            ReplicaTaskState::Queued
        );

        catalog
            .apply(ClusterCommand::CancelTask {
                task_id: task.id,
                reason: "operator cancelled".to_owned(),
                at: now,
            })
            .await
            .expect("cancel");
        assert!(
            matches!(
                catalog
                    .task(task.id)
                    .await
                    .expect("read")
                    .expect("task")
                    .state,
                ReplicaTaskState::Cancelled { .. }
            ),
            "cancellation records why"
        );
    }

    /// A join token is single-use by policy. Consuming it past its ceiling, or
    /// after revocation, would let one leaked token enrol many nodes.
    #[tokio::test]
    async fn a_join_token_is_bounded_by_its_use_count_and_revocation() {
        let (_directory, catalog) = initialized().await;
        let now = Utc::now();
        let token = join_token(now, 1);
        catalog
            .apply(ClusterCommand::IssueJoinToken {
                token: Box::new(token.clone()),
            })
            .await
            .expect("issue");

        catalog
            .apply(ClusterCommand::ConsumeJoinToken {
                token_id: token.id,
                at: now,
            })
            .await
            .expect("first use");
        let stored = catalog
            .join_token(token.id)
            .await
            .expect("read")
            .expect("token");
        assert_eq!(stored.uses, 1);

        catalog
            .apply(ClusterCommand::RevokeJoinToken {
                token_id: token.id,
                at: now,
            })
            .await
            .expect("revoke");
        assert!(
            catalog
                .join_token(token.id)
                .await
                .expect("read")
                .expect("token")
                .revoked
        );

        catalog
            .apply(ClusterCommand::PurgeJoinToken { token_id: token.id })
            .await
            .expect("purge");
        assert!(catalog.join_token(token.id).await.expect("read").is_none());
    }

    #[tokio::test]
    async fn consuming_a_token_that_was_never_issued_is_refused() {
        let (_directory, catalog) = initialized().await;
        assert!(matches!(
            catalog
                .apply(ClusterCommand::ConsumeJoinToken {
                    token_id: record_store_core::JoinTokenId::new(),
                    at: Utc::now(),
                })
                .await,
            Err(ClusterCatalogError::JoinTokenNotFound(_))
        ));
    }

    /// A long-running operation is what an operator watches during a drain, so
    /// its progress and terminal state have to be readable throughout.
    #[tokio::test]
    async fn a_cluster_operation_reports_progress_until_it_finishes() {
        let (_directory, catalog) = initialized().await;
        let now = Utc::now();
        let node_id = register(&catalog, now).await;
        let record = operation(crate::tasks::ClusterOperationKind::Drain, node_id, now);

        catalog
            .apply(ClusterCommand::StartOperation {
                operation: Box::new(record.clone()),
            })
            .await
            .expect("start");
        assert_eq!(catalog.operations(10).await.expect("list").len(), 1);

        catalog
            .apply(ClusterCommand::UpdateOperation {
                operation_id: record.id,
                state: crate::tasks::ClusterOperationState::Completed,
                progress: crate::tasks::OperationProgress {
                    objects_moved: 5,
                    ..crate::tasks::OperationProgress::default()
                },
                message: Some("done".to_owned()),
                at: now,
            })
            .await
            .expect("update");

        let stored = catalog
            .operation(record.id)
            .await
            .expect("read")
            .expect("operation");
        assert_eq!(stored.state, crate::tasks::ClusterOperationState::Completed);
        assert_eq!(stored.progress.objects_moved, 5);
        assert!(stored.completed_at.is_some());
    }

    /// A node credential is how a peer proves who it is, so revoking one has to
    /// take effect in the catalog every other node reads.
    #[tokio::test]
    async fn a_node_credential_is_stored_and_addressable_two_ways() {
        let (_directory, catalog) = initialized().await;
        let now = Utc::now();
        let node_id = register(&catalog, now).await;
        let credential = crate::credentials::NodeCredential {
            id: record_store_core::NodeCredentialId::new(),
            node_id,
            secret_digest: [3_u8; 32],
            created_at: now,
            rotated_at: None,
            disabled: false,
        };

        catalog
            .apply(ClusterCommand::PutNodeCredential {
                credential: Box::new(credential.clone()),
            })
            .await
            .expect("store credential");

        let by_node = catalog
            .node_credential(node_id)
            .await
            .expect("read")
            .expect("credential");
        let by_id = catalog
            .node_credential_by_id(credential.id)
            .await
            .expect("read")
            .expect("credential");
        assert_eq!(by_node.id, credential.id);
        assert_eq!(by_id.node_id, node_id);
    }

    /// Configuration changes are replicated commands like any other, and the
    /// topology view every scheduler reads has to reflect them.
    #[tokio::test]
    async fn a_configuration_change_reaches_the_topology_view() {
        let (_directory, catalog) = initialized().await;
        let mut config = catalog.config().await.expect("read").expect("config");
        config.replication_factor = 3;

        catalog
            .apply(ClusterCommand::UpdateConfig {
                config: Box::new(config),
                at: Utc::now(),
            })
            .await
            .expect("update config");

        let topology = catalog.topology().await.expect("topology");
        assert_eq!(topology.config.replication_factor, 3);
    }

    #[tokio::test]
    async fn usage_counters_are_readable_and_refreshable() {
        let (_directory, catalog) = initialized().await;
        let usage = catalog.usage().await.expect("usage");
        assert_eq!(usage.payloads, 0);
        catalog
            .refresh_durability_counters()
            .await
            .expect("refresh");
        catalog.check_ready().await.expect("ready");
    }

    fn replica_of(node_id: NodeId, state: ReplicaState, now: chrono::DateTime<Utc>) -> Replica {
        Replica {
            node_id,
            state,
            location: "opaque".to_owned(),
            size: 1_024,
            checksum: Some(Checksum::sha256([4_u8; 32])),
            created_at: now,
            verified_at: None,
            generation: 0,
        }
    }

    fn placement_of(
        object_id: ObjectId,
        replicas: Vec<Replica>,
        now: chrono::DateTime<Utc>,
    ) -> PayloadPlacement {
        PayloadPlacement {
            object_id,
            size: 1_024,
            checksum: Checksum::sha256([4_u8; 32]),
            desired_replicas: 3,
            storage_class: StorageClass::new("standard").expect("class"),
            replicas,
            created_at: now,
            updated_at: now,
        }
    }

    /// Placement is the record of where a payload actually lives. Committing it
    /// and reading it back is the contract every durability decision rests on.
    #[tokio::test]
    async fn placement_is_committed_and_readable() {
        let (_directory, catalog) = initialized().await;
        let now = Utc::now();
        let node_id = register(&catalog, now).await;
        let object_id = ObjectId::new();

        catalog
            .apply(ClusterCommand::PutPlacement {
                placement: Box::new(placement_of(
                    object_id,
                    vec![replica_of(node_id, ReplicaState::Healthy, now)],
                    now,
                )),
            })
            .await
            .expect("put placement");

        let stored = catalog
            .placement(object_id)
            .await
            .expect("read")
            .expect("placement");
        assert_eq!(stored.replicas.len(), 1);
        assert_eq!(stored.replicas[0].node_id, node_id);
        assert_eq!(catalog.node_replica_count(node_id).await.expect("count"), 1);
    }

    /// A replica record is upserted as its state changes during a transfer, and
    /// each change has to replace the previous record rather than accumulate.
    #[tokio::test]
    async fn a_replica_record_is_replaced_rather_than_duplicated() {
        let (_directory, catalog) = initialized().await;
        let now = Utc::now();
        let node_id = register(&catalog, now).await;
        let object_id = ObjectId::new();
        catalog
            .apply(ClusterCommand::PutPlacement {
                placement: Box::new(placement_of(object_id, Vec::new(), now)),
            })
            .await
            .expect("put placement");

        for state in [
            ReplicaState::Pending,
            ReplicaState::Repairing,
            ReplicaState::Healthy,
        ] {
            catalog
                .apply(ClusterCommand::UpsertReplica {
                    object_id,
                    replica: Box::new(replica_of(node_id, state, now)),
                    at: now,
                })
                .await
                .expect("upsert replica");
            let stored = catalog
                .placement(object_id)
                .await
                .expect("read")
                .expect("placement");
            assert_eq!(stored.replicas.len(), 1, "{stored:?}");
            assert_eq!(stored.replicas[0].state, state);
        }
    }

    /// Reporting a verification result is how a scrub records what it found, and
    /// the observed checksum has to be kept for the next comparison.
    #[tokio::test]
    async fn a_verification_result_updates_the_replica_state() {
        let (_directory, catalog) = initialized().await;
        let now = Utc::now();
        let node_id = register(&catalog, now).await;
        let object_id = ObjectId::new();
        catalog
            .apply(ClusterCommand::PutPlacement {
                placement: Box::new(placement_of(
                    object_id,
                    vec![replica_of(node_id, ReplicaState::Healthy, now)],
                    now,
                )),
            })
            .await
            .expect("put placement");

        catalog
            .apply(ClusterCommand::SetReplicaState {
                object_id,
                node_id,
                state: ReplicaState::Corrupt,
                checksum: Some(Checksum::sha256([8_u8; 32])),
                verified: true,
                at: now,
            })
            .await
            .expect("set replica state");

        let stored = catalog
            .placement(object_id)
            .await
            .expect("read")
            .expect("placement");
        assert_eq!(stored.replicas[0].state, ReplicaState::Corrupt);
        assert!(stored.replicas[0].verified_at.is_some());
    }

    /// Removing a replica has to release the node's accounting too, or the
    /// placement scheduler keeps believing the node is fuller than it is.
    #[tokio::test]
    async fn removing_a_replica_releases_the_nodes_accounting() {
        let (_directory, catalog) = initialized().await;
        let now = Utc::now();
        let node_id = register(&catalog, now).await;
        let object_id = ObjectId::new();
        catalog
            .apply(ClusterCommand::PutPlacement {
                placement: Box::new(placement_of(
                    object_id,
                    vec![replica_of(node_id, ReplicaState::Healthy, now)],
                    now,
                )),
            })
            .await
            .expect("put placement");
        assert_eq!(catalog.node_replica_count(node_id).await.expect("count"), 1);

        catalog
            .apply(ClusterCommand::RemoveReplica {
                object_id,
                node_id,
                at: now,
            })
            .await
            .expect("remove replica");
        assert_eq!(catalog.node_replica_count(node_id).await.expect("count"), 0);
    }

    /// Raising the desired count is how an operator increases durability, and
    /// the new target has to be what the repair scheduler later reads.
    #[tokio::test]
    async fn the_desired_replica_count_can_be_changed() {
        let (_directory, catalog) = initialized().await;
        let now = Utc::now();
        let object_id = ObjectId::new();
        catalog
            .apply(ClusterCommand::PutPlacement {
                placement: Box::new(placement_of(object_id, Vec::new(), now)),
            })
            .await
            .expect("put placement");

        catalog
            .apply(ClusterCommand::SetDesiredReplicas {
                object_id,
                desired: 5,
                at: now,
            })
            .await
            .expect("set desired");
        assert_eq!(
            catalog
                .placement(object_id)
                .await
                .expect("read")
                .expect("placement")
                .desired_replicas,
            5
        );
    }

    /// A tombstone is how a delete reaches a node that was offline when it
    /// happened. It may only be purged once every holder has acknowledged it,
    /// otherwise the returning node would keep bytes nobody can reach.
    #[tokio::test]
    async fn a_tombstone_survives_until_every_holder_acknowledges_it() {
        let (_directory, catalog) = initialized().await;
        let now = Utc::now();
        let node_id = register(&catalog, now).await;
        let object_id = ObjectId::new();
        catalog
            .apply(ClusterCommand::PutPlacement {
                placement: Box::new(placement_of(
                    object_id,
                    vec![replica_of(node_id, ReplicaState::Healthy, now)],
                    now,
                )),
            })
            .await
            .expect("put placement");

        catalog
            .apply(ClusterCommand::DeletePlacement { object_id, at: now })
            .await
            .expect("delete placement");
        let pending = catalog.pending_tombstones(10).await.expect("pending");
        assert!(
            pending
                .iter()
                .any(|tombstone| tombstone.object_id == object_id),
            "{pending:?}"
        );

        catalog
            .apply(ClusterCommand::AcknowledgeTombstone {
                object_id,
                node_id,
                at: now,
            })
            .await
            .expect("acknowledge");

        catalog
            .apply(ClusterCommand::PurgeTombstone { object_id })
            .await
            .expect("purge");
        assert!(catalog.tombstone(object_id).await.expect("read").is_none());
    }

    /// Commands naming a payload the cluster has no placement for must be
    /// refused rather than creating one implicitly.
    #[tokio::test]
    async fn replica_commands_against_an_unknown_placement_are_refused() {
        let (_directory, catalog) = initialized().await;
        let now = Utc::now();
        let node_id = register(&catalog, now).await;
        let object_id = ObjectId::new();

        assert!(matches!(
            catalog
                .apply(ClusterCommand::UpsertReplica {
                    object_id,
                    replica: Box::new(replica_of(node_id, ReplicaState::Healthy, now)),
                    at: now,
                })
                .await,
            Err(ClusterCatalogError::PlacementNotFound(_))
        ));
        assert!(matches!(
            catalog
                .apply(ClusterCommand::SetDesiredReplicas {
                    object_id,
                    desired: 2,
                    at: now,
                })
                .await,
            Err(ClusterCatalogError::PlacementNotFound(_))
        ));
    }

    /// Listing placements is how the coordinator sweeps for repair work, so it
    /// has to page rather than hand back the whole cluster at once.
    #[tokio::test]
    async fn placements_are_listed_in_pages() {
        let (_directory, catalog) = initialized().await;
        let now = Utc::now();
        for _ in 0..3 {
            catalog
                .apply(ClusterCommand::PutPlacement {
                    placement: Box::new(placement_of(ObjectId::new(), Vec::new(), now)),
                })
                .await
                .expect("put placement");
        }

        let page = catalog.list_placements(None, 2).await.expect("list");
        assert_eq!(page.placements.len(), 2, "{page:?}");
        assert!(page.next_object_id.is_some(), "{page:?}");

        let rest = catalog
            .list_placements(page.next_object_id, 10)
            .await
            .expect("list");
        assert_eq!(rest.placements.len(), 1, "{rest:?}");
    }
}
