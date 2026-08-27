//! Replica placement metadata and durability accounting.
//!
//! Placement is tracked per immutable payload identifier, because a payload is
//! what a node physically stores. Object versions, delete markers, and multipart
//! parts all reference payloads, so one placement model covers every case
//! without duplicating version semantics.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use record_store_core::{Checksum, NodeId, ObjectId};
use serde::{Deserialize, Serialize};

use crate::topology::{ClusterTopology, FailureDomainScope, StorageClass};

/// Explicit state of one physical replica.
///
/// Replica health is tracked independently of node health: a perfectly healthy
/// node can still hold a corrupt or missing replica.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplicaState {
    /// Bytes are being written and the replica is not yet durable.
    Pending,
    /// Bytes are durable and verified against the expected checksum.
    Healthy,
    /// A transfer is actively rebuilding this replica.
    Repairing,
    /// The replica exists but is known to be out of date.
    Stale,
    /// The node reported that the payload is absent.
    Missing,
    /// The replica is scheduled for removal.
    Deleting,
    /// Stored bytes failed integrity verification.
    Corrupt,
}

impl ReplicaState {
    /// Returns whether this replica may be served to a client.
    #[must_use]
    pub const fn readable(self) -> bool {
        matches!(self, Self::Healthy)
    }

    /// Returns whether this replica counts towards durability.
    #[must_use]
    pub const fn durable(self) -> bool {
        matches!(self, Self::Healthy)
    }

    /// Returns whether this replica may be used as a repair source.
    #[must_use]
    pub const fn usable_as_source(self) -> bool {
        matches!(self, Self::Healthy)
    }

    /// Returns whether the replica is known to need repair.
    #[must_use]
    pub const fn needs_repair(self) -> bool {
        matches!(self, Self::Missing | Self::Corrupt | Self::Stale)
    }
}

/// One physical replica of a payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replica {
    /// Node holding the bytes.
    pub node_id: NodeId,
    /// Current replica state.
    pub state: ReplicaState,
    /// Opaque location identifier owned by the storing node.
    ///
    /// This is never a filesystem path and is never exposed through public APIs.
    pub location: String,
    /// Size of the stored payload in bytes.
    pub size: u64,
    /// Checksum the node last confirmed for the stored bytes.
    pub checksum: Option<Checksum>,
    /// Time the replica record was created.
    pub created_at: DateTime<Utc>,
    /// Last time the node verified the stored bytes.
    pub verified_at: Option<DateTime<Utc>>,
    /// Monotonic counter incremented by each successful rebuild.
    pub generation: u64,
}

impl Replica {
    /// Creates a pending replica record for a placement target.
    #[must_use]
    pub fn pending(node_id: NodeId, size: u64, now: DateTime<Utc>) -> Self {
        Self {
            node_id,
            state: ReplicaState::Pending,
            location: opaque_location(node_id),
            size,
            checksum: None,
            created_at: now,
            verified_at: None,
            generation: 0,
        }
    }

    /// Creates a verified replica record.
    #[must_use]
    pub fn healthy(node_id: NodeId, size: u64, checksum: Checksum, now: DateTime<Utc>) -> Self {
        Self {
            node_id,
            state: ReplicaState::Healthy,
            location: opaque_location(node_id),
            size,
            checksum: Some(checksum),
            created_at: now,
            verified_at: Some(now),
            generation: 1,
        }
    }
}

fn opaque_location(node_id: NodeId) -> String {
    format!("node:{}", node_id.as_uuid().simple())
}

/// Replicated placement metadata for one immutable payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayloadPlacement {
    /// Immutable payload identifier.
    pub object_id: ObjectId,
    /// Payload length in bytes.
    pub size: u64,
    /// Authoritative payload checksum recorded at commit time.
    pub checksum: Checksum,
    /// Desired number of durable replicas.
    pub desired_replicas: u8,
    /// Storage class replicas must live on.
    pub storage_class: StorageClass,
    /// Known replicas ordered by node identifier.
    pub replicas: Vec<Replica>,
    /// Time the placement was first committed.
    pub created_at: DateTime<Utc>,
    /// Time the placement was last modified.
    pub updated_at: DateTime<Utc>,
}

impl PayloadPlacement {
    /// Creates a placement record with deterministic replica ordering.
    #[must_use]
    pub fn new(
        object_id: ObjectId,
        size: u64,
        checksum: Checksum,
        desired_replicas: u8,
        storage_class: StorageClass,
        mut replicas: Vec<Replica>,
        now: DateTime<Utc>,
    ) -> Self {
        replicas.sort_by_key(|replica| replica.node_id);
        replicas.dedup_by_key(|replica| replica.node_id);
        Self {
            object_id,
            size,
            checksum,
            desired_replicas,
            storage_class,
            replicas,
            created_at: now,
            updated_at: now,
        }
    }

    /// Returns the replica stored on a node.
    #[must_use]
    pub fn replica(&self, node_id: NodeId) -> Option<&Replica> {
        self.replicas
            .iter()
            .find(|replica| replica.node_id == node_id)
    }

    /// Inserts or replaces a replica, keeping deterministic order.
    pub fn upsert_replica(&mut self, replica: Replica, now: DateTime<Utc>) {
        match self
            .replicas
            .iter_mut()
            .find(|existing| existing.node_id == replica.node_id)
        {
            Some(existing) => *existing = replica,
            None => {
                self.replicas.push(replica);
                self.replicas.sort_by_key(|replica| replica.node_id);
            }
        }
        self.updated_at = now;
    }

    /// Removes a replica record, returning whether anything was removed.
    pub fn remove_replica(&mut self, node_id: NodeId, now: DateTime<Utc>) -> bool {
        let before = self.replicas.len();
        self.replicas.retain(|replica| replica.node_id != node_id);
        let removed = self.replicas.len() != before;
        if removed {
            self.updated_at = now;
        }
        removed
    }

    /// Returns every node holding a replica record of any state.
    #[must_use]
    pub fn nodes(&self) -> BTreeSet<NodeId> {
        self.replicas
            .iter()
            .map(|replica| replica.node_id)
            .collect()
    }

    /// Returns durability accounting against a topology view.
    #[must_use]
    pub fn durability(&self, topology: &ClusterTopology) -> Durability {
        let mut current = 0_u32;
        let mut healthy = 0_u32;
        let mut readable = 0_u32;
        let mut unavailable = 0_u32;
        let mut damaged = 0_u32;
        for replica in &self.replicas {
            current = current.saturating_add(1);
            let node_counts = topology.contributes_durability(replica.node_id);
            if replica.state.durable() && node_counts {
                healthy = healthy.saturating_add(1);
            }
            if replica.state.readable() && topology.serves_reads(replica.node_id) {
                readable = readable.saturating_add(1);
            }
            if !node_counts {
                unavailable = unavailable.saturating_add(1);
            } else if replica.state.needs_repair() {
                damaged = damaged.saturating_add(1);
            }
        }
        Durability {
            desired: u32::from(self.desired_replicas),
            current,
            healthy,
            readable,
            unavailable,
            damaged,
        }
    }

    /// Returns the distinct failure domains currently holding durable replicas.
    #[must_use]
    pub fn occupied_domains(
        &self,
        topology: &ClusterTopology,
        scope: FailureDomainScope,
    ) -> BTreeMap<String, u32> {
        let mut domains = BTreeMap::new();
        for replica in &self.replicas {
            if !replica.state.durable() {
                continue;
            }
            if let Some(node) = topology.node(replica.node_id) {
                *domains.entry(node.domain_key(scope)).or_insert(0_u32) += 1;
            }
        }
        domains
    }

    /// Returns whether replicas violate the configured failure-domain spread.
    #[must_use]
    pub fn violates_failure_domains(
        &self,
        topology: &ClusterTopology,
        scope: FailureDomainScope,
    ) -> bool {
        let domains = self.occupied_domains(topology, scope);
        let durable: u32 = domains.values().copied().sum();
        durable > 1 && u32::try_from(domains.len()).unwrap_or(u32::MAX) < durable
    }
}

/// Separate desired, current, and healthy replica accounting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Durability {
    /// Replicas the policy asks for.
    pub desired: u32,
    /// Replica records that exist in metadata.
    pub current: u32,
    /// Replicas that are durable on nodes that still count.
    pub healthy: u32,
    /// Replicas that can be streamed to a client right now.
    pub readable: u32,
    /// Replica records on nodes that no longer count towards durability.
    pub unavailable: u32,
    /// Replica records that are missing, stale, or corrupt on live nodes.
    pub damaged: u32,
}

impl Durability {
    /// Returns whether the payload has fewer healthy replicas than desired.
    #[must_use]
    pub const fn under_replicated(&self) -> bool {
        self.healthy < self.desired
    }

    /// Returns whether no healthy replica remains.
    #[must_use]
    pub const fn unavailable(&self) -> bool {
        self.healthy == 0
    }

    /// Returns how many replicas are missing relative to the desired count.
    #[must_use]
    pub const fn deficit(&self) -> u32 {
        self.desired.saturating_sub(self.healthy)
    }
}

/// A durable deletion record.
///
/// Tombstones are what stop a node that was offline during a delete from
/// resurrecting the payload when it returns. They are retained until every node
/// that could still hold the bytes has confirmed removal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tombstone {
    /// Payload that must not exist anywhere.
    pub object_id: ObjectId,
    /// Time the deletion was committed.
    pub created_at: DateTime<Utc>,
    /// Nodes that have not yet confirmed removal.
    pub pending_nodes: BTreeSet<NodeId>,
    /// Nodes that have confirmed removal.
    pub completed_nodes: BTreeSet<NodeId>,
    /// Time every known node had confirmed removal.
    pub completed_at: Option<DateTime<Utc>>,
}

impl Tombstone {
    /// Creates a tombstone for every node that may still hold the payload.
    #[must_use]
    pub fn new(object_id: ObjectId, nodes: BTreeSet<NodeId>, now: DateTime<Utc>) -> Self {
        let completed_at = nodes.is_empty().then_some(now);
        Self {
            object_id,
            created_at: now,
            pending_nodes: nodes,
            completed_nodes: BTreeSet::new(),
            completed_at,
        }
    }

    /// Records that a node removed its copy.
    pub fn acknowledge(&mut self, node_id: NodeId, now: DateTime<Utc>) {
        self.pending_nodes.remove(&node_id);
        self.completed_nodes.insert(node_id);
        if self.pending_nodes.is_empty() && self.completed_at.is_none() {
            self.completed_at = Some(now);
        }
    }

    /// Returns whether every known holder has confirmed removal.
    #[must_use]
    pub const fn completed(&self) -> bool {
        self.completed_at.is_some()
    }

    /// Returns whether the tombstone may be purged.
    ///
    /// Purging early would let a returning node resurrect deleted data, so the
    /// record is kept for the configured retention window after completion.
    #[must_use]
    pub fn purgeable(&self, retention_hours: u32, now: DateTime<Utc>) -> bool {
        match self.completed_at {
            Some(completed) => {
                let retention = chrono::TimeDelta::try_hours(i64::from(retention_hours))
                    .unwrap_or_else(chrono::TimeDelta::zero);
                now.signed_duration_since(completed) >= retention
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;
    use record_store_core::ClusterId;

    use super::*;
    use crate::{
        config::ClusterConfig,
        topology::{FailureDomain, NodeCapacity, NodeRecord, NodeState},
        version::ProtocolVersion,
    };

    fn checksum() -> Checksum {
        Checksum::sha256([7_u8; 32])
    }

    fn node(state: NodeState, domain: &str) -> NodeRecord {
        NodeRecord {
            node_id: NodeId::new(),
            raft_id: 1,
            protocol: ProtocolVersion::current(),
            software_version: "test".into(),
            storage_format_version: 1,
            rpc_address: "127.0.0.1:7603".into(),
            s3_endpoint: None,
            storage_class: StorageClass::default(),
            failure_domain: FailureDomain::parse(&format!("rack={domain}")).expect("labels"),
            state,
            metadata_voter: true,
            capacity: NodeCapacity {
                total_bytes: 1_000,
                available_bytes: 900,
                replica_bytes: 100,
                temporary_bytes: 0,
            },
            activity: crate::topology::NodeActivity::default(),
            joined_at: Utc::now(),
            started_at: Utc::now(),
            last_heartbeat_at: Some(Utc::now()),
            state_changed_at: Utc::now(),
            state_reason: None,
        }
    }

    #[test]
    fn durability_separates_desired_current_and_healthy() {
        let healthy_node = node(NodeState::Healthy, "a");
        let offline_node = node(NodeState::Offline, "b");
        let corrupt_node = node(NodeState::Healthy, "c");
        let topology = ClusterTopology::new(
            ClusterId::new(),
            ClusterConfig::default(),
            vec![
                healthy_node.clone(),
                offline_node.clone(),
                corrupt_node.clone(),
            ],
        );
        let now = Utc::now();
        let mut placement = PayloadPlacement::new(
            ObjectId::new(),
            10,
            checksum(),
            3,
            StorageClass::default(),
            vec![
                Replica::healthy(healthy_node.node_id, 10, checksum(), now),
                Replica::healthy(offline_node.node_id, 10, checksum(), now),
            ],
            now,
        );
        let mut corrupt = Replica::healthy(corrupt_node.node_id, 10, checksum(), now);
        corrupt.state = ReplicaState::Corrupt;
        placement.upsert_replica(corrupt, now);

        let durability = placement.durability(&topology);
        assert_eq!(durability.desired, 3);
        assert_eq!(durability.current, 3);
        assert_eq!(durability.healthy, 1);
        assert_eq!(durability.readable, 1);
        assert_eq!(durability.unavailable, 1);
        assert_eq!(durability.damaged, 1);
        assert!(durability.under_replicated());
        assert!(!durability.unavailable());
        assert_eq!(durability.deficit(), 2);
    }

    #[test]
    fn tombstones_complete_only_after_every_holder_confirms() {
        let first = NodeId::new();
        let second = NodeId::new();
        let now = Utc::now();
        let mut tombstone = Tombstone::new(ObjectId::new(), BTreeSet::from([first, second]), now);
        assert!(!tombstone.completed());
        tombstone.acknowledge(first, now);
        assert!(!tombstone.completed());
        assert!(!tombstone.purgeable(1, now + TimeDelta::try_hours(100).expect("delta")));
        tombstone.acknowledge(second, now);
        assert!(tombstone.completed());
        assert!(!tombstone.purgeable(1, now));
        assert!(tombstone.purgeable(1, now + TimeDelta::try_hours(2).expect("delta")));
    }

    #[test]
    fn failure_domain_violation_is_detected() {
        let first = node(NodeState::Healthy, "same");
        let second = node(NodeState::Healthy, "same");
        let topology = ClusterTopology::new(
            ClusterId::new(),
            ClusterConfig::default(),
            vec![first.clone(), second.clone()],
        );
        let now = Utc::now();
        let placement = PayloadPlacement::new(
            ObjectId::new(),
            10,
            checksum(),
            2,
            StorageClass::default(),
            vec![
                Replica::healthy(first.node_id, 10, checksum(), now),
                Replica::healthy(second.node_id, 10, checksum(), now),
            ],
            now,
        );
        assert!(placement.violates_failure_domains(&topology, FailureDomainScope::Rack));
        assert!(!placement.violates_failure_domains(&topology, FailureDomainScope::Node));
    }
}
