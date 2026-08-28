//! Shared fixtures for placement and topology tests.

use chrono::{DateTime, Utc};
use record_store_cluster::{
    ClusterConfig, ClusterTopology, FailureDomain, NodeCapacity, NodeRecord, NodeRegistration,
    NodeState, NodeVersions, PayloadPlacement, Replica, ReplicaState, StorageClass,
};
use record_store_core::{Checksum, ClusterId, NodeId, ObjectId};

/// Builds a replica record in the requested state.
pub(crate) fn replica(node_id: NodeId, state: ReplicaState) -> Replica {
    Replica {
        node_id,
        state,
        location: "opaque".to_owned(),
        size: 1_024,
        checksum: Some(Checksum::sha256([1_u8; 32])),
        created_at: timestamp(),
        verified_at: None,
        generation: 0,
    }
}

/// Builds a placement holding the supplied replicas.
pub(crate) fn placement(object_id: ObjectId, replicas: Vec<Replica>) -> PayloadPlacement {
    PayloadPlacement {
        object_id,
        size: 1_024,
        checksum: Checksum::sha256([1_u8; 32]),
        desired_replicas: 3,
        storage_class: StorageClass::new("standard").expect("storage class"),
        replicas,
        created_at: timestamp(),
        updated_at: timestamp(),
    }
}

/// Builds a node record in the requested lifecycle state.
pub(crate) fn node(node_id: NodeId, raft_id: u64, state: NodeState) -> NodeRecord {
    let registration = NodeRegistration {
        node_id,
        versions: NodeVersions::current("test"),
        rpc_address: format!("127.0.0.1:{}", 7_603 + raft_id),
        s3_endpoint: None,
        storage_class: StorageClass::new("standard").expect("storage class"),
        failure_domain: FailureDomain::default(),
        capacity: NodeCapacity::default(),
        started_at: timestamp(),
    };
    let mut record = NodeRecord::joining(registration, raft_id, true, timestamp());
    record.state = state;
    record
}

/// Builds a topology view over the supplied nodes.
pub(crate) fn topology(nodes: Vec<NodeRecord>) -> ClusterTopology {
    ClusterTopology::new(ClusterId::new(), ClusterConfig::default(), nodes)
}

/// A fixed instant, so fixtures never depend on wall-clock ordering.
pub(crate) fn timestamp() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).expect("valid timestamp")
}

/// Builds a movement task between two nodes.
pub(crate) fn task(
    kind: record_store_cluster::ReplicaTaskKind,
    source: NodeId,
    target: NodeId,
) -> record_store_cluster::ReplicaTask {
    record_store_cluster::ReplicaTask {
        id: record_store_core::ReplicaTaskId::new(),
        object_id: ObjectId::new(),
        kind,
        priority: record_store_cluster::ReplicaTaskPriority::Low,
        source_node: Some(source),
        target_node: Some(target),
        operation_id: None,
        size: 1_024,
        state: record_store_cluster::ReplicaTaskState::Queued,
        attempts: 0,
        last_error: None,
        created_at: timestamp(),
        updated_at: timestamp(),
    }
}
