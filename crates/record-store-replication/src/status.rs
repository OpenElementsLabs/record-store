//! Cluster status reporting.
//!
//! Status is assembled from counters that the cluster already maintains rather
//! than by scanning every object, so asking a healthy cluster how it is doing
//! stays cheap. Data availability and metadata availability are reported as
//! separate dimensions, because losing either one has different consequences.

use std::{collections::BTreeMap, sync::Arc};

use chrono::{DateTime, Utc};
use record_store_cluster::{
    ClusterHealth, ClusterOperation, ClusterUsage, DataHealth, NodeState, QuorumStatus,
};
use record_store_consensus::{MetadataConsensus, MetadataQuorum};
use record_store_core::{ClusterId, NodeId};
use serde::{Deserialize, Serialize};

use crate::{context::ClusterContext, runtime::TaskStatus};

/// One member's operator-facing status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeStatus {
    /// Stable node identity.
    pub node_id: NodeId,
    /// Consensus member identifier.
    pub member_id: u64,
    /// Lifecycle state.
    pub state: NodeState,
    /// Whether the node votes in metadata consensus.
    pub metadata_voter: bool,
    /// Internal RPC address.
    pub rpc_address: String,
    /// Storage class.
    pub storage_class: String,
    /// Failure-domain labels.
    pub failure_domain: BTreeMap<String, String>,
    /// Build version last advertised.
    pub software_version: String,
    /// Total filesystem capacity.
    pub capacity_bytes: u64,
    /// Free filesystem capacity.
    pub available_bytes: u64,
    /// Utilization in whole percent.
    pub utilization_percent: u32,
    /// Replica records the node holds.
    pub replicas: u64,
    /// Last accepted heartbeat.
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    /// When the current state was entered.
    pub state_changed_at: DateTime<Utc>,
    /// Operator-facing reason for the current state.
    pub state_reason: Option<String>,
}

/// Replication and repair progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicationStatus {
    /// Desired replicas per payload.
    pub replication_factor: u8,
    /// Replicas required before a write is acknowledged.
    pub required_acknowledgements: u8,
    /// Payloads tracked by the cluster.
    pub payloads: u64,
    /// Logical bytes stored once.
    pub logical_bytes: u64,
    /// Physical bytes across all replicas.
    pub physical_bytes: u64,
    /// Payloads below their desired replica count.
    pub under_replicated_payloads: u64,
    /// Payloads with no healthy replica.
    pub unavailable_payloads: u64,
    /// Outstanding tombstones.
    pub tombstones: u64,
}

/// Repair and movement queue depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairStatus {
    /// Movement tasks still to run.
    pub active_tasks: u64,
    /// Tasks parked after exhausting their retries.
    pub parked_tasks: u64,
}

/// The complete cluster status document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterStatus {
    /// Cluster identity.
    pub cluster_id: ClusterId,
    /// Overall health, the worse of the data and metadata dimensions.
    pub health: ClusterHealth,
    /// Metadata consensus health.
    pub metadata: MetadataQuorum,
    /// Data-plane health.
    pub data: DataHealth,
    /// Replication accounting.
    pub replication: ReplicationStatus,
    /// Movement queue depth.
    pub repair: RepairStatus,
    /// Members.
    pub nodes: Vec<NodeStatus>,
    /// Active long-running operations.
    pub operations: Vec<ClusterOperation>,
    /// Liveness of this node's supervised background services.
    pub local_tasks: BTreeMap<String, TaskStatus>,
    /// Time the status was assembled.
    pub observed_at: DateTime<Utc>,
}

impl ClusterStatus {
    /// Assembles the status document from maintained counters.
    pub async fn collect(
        context: &ClusterContext,
        consensus: &Arc<MetadataConsensus>,
        local_tasks: BTreeMap<String, TaskStatus>,
    ) -> Result<Self, String> {
        let topology = context.topology().await.map_err(display)?;
        let usage: ClusterUsage = context.cluster.usage().await.map_err(display)?;
        let metadata = consensus.quorum().await;
        let data = DataHealth::evaluate(
            &topology,
            usage.under_replicated_payloads,
            usage.unavailable_payloads,
        );
        let mut nodes = Vec::with_capacity(topology.nodes.len());
        for node in &topology.nodes {
            let replicas = context
                .cluster
                .node_replica_count(node.node_id)
                .await
                .unwrap_or(0);
            nodes.push(NodeStatus {
                node_id: node.node_id,
                member_id: node.raft_id,
                state: node.state,
                metadata_voter: node.metadata_voter,
                rpc_address: node.rpc_address.clone(),
                storage_class: node.storage_class.to_string(),
                failure_domain: node.failure_domain.labels().clone(),
                software_version: node.software_version.clone(),
                capacity_bytes: node.capacity.total_bytes,
                available_bytes: node.capacity.available_bytes,
                utilization_percent: node.capacity.utilization_percent(),
                replicas,
                last_heartbeat_at: node.last_heartbeat_at,
                state_changed_at: node.state_changed_at,
                state_reason: node.state_reason.clone(),
            });
        }
        let operations = context
            .cluster
            .operations(32)
            .await
            .map_err(display)?
            .into_iter()
            .filter(|operation| operation.state.active())
            .collect();
        let health = data.health.worst(metadata.status.health);
        Ok(Self {
            cluster_id: topology.cluster_id,
            health,
            metadata,
            data,
            replication: ReplicationStatus {
                replication_factor: topology.config.replication_factor,
                required_acknowledgements: topology.config.required_acknowledgements(),
                payloads: usage.payloads,
                logical_bytes: usage.logical_bytes,
                physical_bytes: usage.physical_bytes,
                under_replicated_payloads: usage.under_replicated_payloads,
                unavailable_payloads: usage.unavailable_payloads,
                tombstones: usage.tombstones,
            },
            repair: RepairStatus {
                active_tasks: usage.active_tasks,
                parked_tasks: usage.parked_tasks,
            },
            nodes,
            operations,
            local_tasks,
            observed_at: Utc::now(),
        })
    }

    /// Returns the health reasons an operator should act on.
    #[must_use]
    pub fn reasons(&self) -> Vec<String> {
        let mut reasons = self.metadata.status.notes.clone();
        reasons.extend(self.data.notes.clone());
        for (name, status) in &self.local_tasks {
            if let TaskStatus::Failed { reason, .. } = status {
                reasons.push(format!("background task '{name}' stopped: {reason}"));
            }
        }
        reasons
    }

    /// Returns the metadata quorum summary.
    #[must_use]
    pub const fn quorum(&self) -> &QuorumStatus {
        &self.metadata.status
    }
}

fn display<E: std::fmt::Display>(error: E) -> String {
    error.to_string()
}
