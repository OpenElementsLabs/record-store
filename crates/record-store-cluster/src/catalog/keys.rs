use chrono::{DateTime, Utc};
use record_store_core::{DeviceId, NodeId, ObjectId};

use crate::{
    replica::ReplicaState,
    tasks::{ReplicaTask, ReplicaTaskKind},
};

pub(crate) fn task_queue_key(task: &ReplicaTask) -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + 8 + 16);
    key.push(task.priority.rank());
    key.extend_from_slice(&ordered_timestamp(task.created_at));
    key.extend_from_slice(task.id.as_uuid().as_bytes());
    key
}

pub(crate) fn task_object_key(object_id: ObjectId, kind: ReplicaTaskKind) -> Vec<u8> {
    let mut key = Vec::with_capacity(17);
    key.extend_from_slice(object_id.as_uuid().as_bytes());
    key.push(task_kind_code(kind));
    key
}

pub(crate) fn node_replica_key(
    node_id: NodeId,
    object_id: ObjectId,
    device_id: DeviceId,
) -> Vec<u8> {
    let mut key = Vec::with_capacity(48);
    key.extend_from_slice(node_id.as_uuid().as_bytes());
    key.extend_from_slice(object_id.as_uuid().as_bytes());
    key.extend_from_slice(device_id.as_uuid().as_bytes());
    key
}

pub(crate) const fn task_kind_code(kind: ReplicaTaskKind) -> u8 {
    match kind {
        ReplicaTaskKind::Repair => 0,
        ReplicaTaskKind::RepairCorrupt => 1,
        ReplicaTaskKind::Drain => 2,
        ReplicaTaskKind::Rebalance => 3,
        ReplicaTaskKind::RebalanceDomain => 4,
        ReplicaTaskKind::Delete => 5,
    }
}

pub(crate) const fn replica_state_code(state: ReplicaState) -> u8 {
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

pub(crate) fn ordered_timestamp(value: DateTime<Utc>) -> [u8; 8] {
    // Flip the sign bit so that byte ordering matches numeric ordering.
    ((value.timestamp_millis() as u64) ^ (1_u64 << 63)).to_be_bytes()
}

pub(crate) fn prefix_successor(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last != u8::MAX {
            end.push(last + 1);
            return end;
        }
    }
    vec![u8::MAX; prefix.len() + 1]
}
