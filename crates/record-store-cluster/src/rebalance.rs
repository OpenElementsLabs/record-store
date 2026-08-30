//! Rebalance planning and destructive-operation safety checks.
//!
//! Both problems are pure functions over a topology view and a set of placement
//! records, which is what makes them testable without a running cluster.

use std::collections::{BTreeMap, BTreeSet};

use record_store_core::{DeviceId, NodeId, ObjectId};
use serde::{Deserialize, Serialize};

use crate::{
    config::CapacityLevel,
    placement::{ObjectPlacementRequest, PlacementPolicy},
    replica::PayloadPlacement,
    topology::{ClusterTopology, NodeState},
};

/// One payload considered for movement.
#[derive(Debug, Clone)]
pub struct RebalanceCandidate {
    /// Placement metadata for the payload.
    pub placement: PayloadPlacement,
}

/// A single planned replica move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebalanceMove {
    /// Payload to move.
    pub object_id: ObjectId,
    /// Node the replica is released from, after the target is verified.
    pub source_node: NodeId,
    /// Exact source device.
    pub source_device: DeviceId,
    /// Node the replica is copied to.
    pub target_node: NodeId,
    /// Exact destination device.
    pub target_device: DeviceId,
    /// Payload size, used for progress accounting.
    pub size: u64,
}

/// Plans capacity-driven replica movement.
///
/// The algorithm is deliberately the simplest one that is correct and
/// explainable: find nodes above the tolerated spread, find nodes below it, and
/// move replicas from the former to the latter while preserving the replica count
/// and failure-domain spread. It never removes a source replica; that only
/// happens after the destination replica is written, verified, and committed.
#[must_use]
pub fn plan_rebalance(
    topology: &ClusterTopology,
    candidates: &[RebalanceCandidate],
    policy: &dyn PlacementPolicy,
    maximum_moves: usize,
) -> Vec<RebalanceMove> {
    let members: Vec<_> = topology
        .nodes
        .iter()
        .filter(|node| node.state == NodeState::Healthy)
        .collect();
    if members.len() < 2 || maximum_moves == 0 {
        return Vec::new();
    }
    let total: u64 = members
        .iter()
        .map(|node| u64::from(node.capacity.utilization_percent()))
        .sum();
    let mean = total / u64::try_from(members.len()).unwrap_or(1).max(1);
    let tolerance = u64::from(topology.config.rebalance.tolerance_percent);

    let donors: BTreeSet<NodeId> = members
        .iter()
        .filter(|node| {
            let utilization = u64::from(node.capacity.utilization_percent());
            utilization > mean.saturating_add(tolerance)
                || node.capacity_level(&topology.config).needs_relief()
        })
        .map(|node| node.node_id)
        .collect();
    let recipients: BTreeSet<NodeId> = members
        .iter()
        .filter(|node| {
            let utilization = u64::from(node.capacity.utilization_percent());
            utilization + tolerance < mean
                && node.capacity_level(&topology.config) == CapacityLevel::Normal
        })
        .map(|node| node.node_id)
        .collect();
    if donors.is_empty() || recipients.is_empty() {
        return Vec::new();
    }

    let mut moves = Vec::new();
    // Track planned arrivals so one pass does not overfill a single recipient.
    let mut arrivals: BTreeMap<NodeId, usize> = BTreeMap::new();
    let fair_share = candidates.len().div_ceil(recipients.len().max(1)).max(1);

    for candidate in candidates {
        if moves.len() >= maximum_moves {
            break;
        }
        let placement = &candidate.placement;
        let holders = placement.nodes();
        let Some(source) = placement
            .replicas
            .iter()
            .find(|replica| donors.contains(&replica.node_id))
        else {
            continue;
        };
        if placement
            .replica_on(source.node_id, source.device_id)
            .is_none_or(|replica| !replica.state.usable_as_source())
        {
            continue;
        }
        // Placement counts existing replicas towards the desired total, so ask
        // for exactly one more than the payload already has.
        let desired = u8::try_from(holders.len().saturating_add(1)).unwrap_or(u8::MAX);
        let request = ObjectPlacementRequest::new(
            placement.object_id,
            desired,
            1,
            placement.storage_class.clone(),
        )
        .with_size_hint(Some(placement.size))
        .with_existing_nodes(holders.iter().copied())
        .with_excluded_nodes(
            topology
                .nodes
                .iter()
                .map(|node| node.node_id)
                .filter(|node_id| {
                    !recipients.contains(node_id)
                        || arrivals.get(node_id).copied().unwrap_or(0) >= fair_share
                }),
        );
        let Ok(plan) = policy.place(&request, topology) else {
            continue;
        };
        let Some(target) = plan.targets.first() else {
            continue;
        };
        *arrivals.entry(target.node_id).or_insert(0) += 1;
        moves.push(RebalanceMove {
            object_id: placement.object_id,
            source_node: source.node_id,
            source_device: source.device_id,
            target_node: target.node_id,
            target_device: target.device_id,
            size: placement.size,
        });
    }
    moves
}

/// The result of checking whether a node can be removed safely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecommissionSafety {
    /// Node the check was performed for.
    pub node_id: NodeId,
    /// Whether removal preserves the required durability for every payload.
    pub safe: bool,
    /// Payload versions that would fall below the required durability.
    pub at_risk_payloads: u64,
    /// Payload versions that would become completely unreadable.
    pub unavailable_payloads: u64,
    /// Replica records the node still holds.
    pub replicas_remaining: u64,
    /// Bytes the node still holds.
    pub bytes_remaining: u64,
    /// Human-readable explanation for the decision.
    pub reason: String,
}

impl DecommissionSafety {
    /// Evaluates whether removing a node preserves durability.
    ///
    /// The check is intentionally pessimistic: a payload whose remaining healthy
    /// replica count would drop below the write policy's requirement counts as at
    /// risk, even if repair could later restore it, because the operator is about
    /// to remove the bytes right now.
    #[must_use]
    pub fn evaluate(
        topology: &ClusterTopology,
        node_id: NodeId,
        placements: &[PayloadPlacement],
    ) -> Self {
        let required = u32::from(topology.config.required_acknowledgements());
        let mut at_risk = 0_u64;
        let mut unavailable = 0_u64;
        let mut replicas = 0_u64;
        let mut bytes = 0_u64;
        for placement in placements {
            let Some(replica) = placement.replica(node_id) else {
                continue;
            };
            replicas = replicas.saturating_add(1);
            bytes = bytes.saturating_add(replica.size);
            let durability = placement.durability(topology);
            let remaining = if replica.state.durable() && topology.contributes_durability(node_id) {
                durability.healthy.saturating_sub(1)
            } else {
                durability.healthy
            };
            if remaining == 0 {
                unavailable = unavailable.saturating_add(1);
            } else if remaining < required.min(durability.desired) {
                at_risk = at_risk.saturating_add(1);
            }
        }
        let safe = at_risk == 0 && unavailable == 0;
        let reason = if safe {
            if replicas == 0 {
                format!("node {node_id} holds no replicas and can be removed")
            } else {
                format!(
                    "removing node {node_id} keeps every payload at or above the required \
                     durability of {required} replica(s)"
                )
            }
        } else {
            format!(
                "{unavailable} object version(s) would become unreadable and \
                 {at_risk} would fall below the required durability of {required} replica(s)"
            )
        };
        Self {
            node_id,
            safe,
            at_risk_payloads: at_risk,
            unavailable_payloads: unavailable,
            replicas_remaining: replicas,
            bytes_remaining: bytes,
            reason,
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use record_store_core::{Checksum, ClusterId};

    use super::*;
    use crate::{
        config::ClusterConfig,
        placement::CapacityAwarePlacement,
        replica::Replica,
        topology::{FailureDomain, NodeActivity, NodeCapacity, NodeRecord, StorageClass},
        version::ProtocolVersion,
    };

    fn node(rack: &str, total: u64, available: u64) -> NodeRecord {
        NodeRecord {
            node_id: NodeId::new(),
            raft_id: 1,
            protocol: ProtocolVersion::current(),
            software_version: "test".into(),
            storage_format_version: 1,
            rpc_address: "127.0.0.1:7603".into(),
            s3_endpoint: None,
            storage_class: StorageClass::default(),
            failure_domain: FailureDomain::parse(&format!("rack={rack}")).expect("labels"),
            state: NodeState::Healthy,
            metadata_voter: false,
            capacity: NodeCapacity {
                total_bytes: total,
                available_bytes: available,
                replica_bytes: total.saturating_sub(available),
                temporary_bytes: 0,
            },
            devices: Vec::new(),
            activity: NodeActivity::default(),
            joined_at: Utc::now(),
            started_at: Utc::now(),
            last_heartbeat_at: Some(Utc::now()),
            state_changed_at: Utc::now(),
            state_reason: None,
        }
    }

    fn placement(nodes: &[NodeId], desired: u8) -> PayloadPlacement {
        let now = Utc::now();
        PayloadPlacement::new(
            ObjectId::new(),
            100,
            Checksum::sha256([3_u8; 32]),
            desired,
            StorageClass::default(),
            nodes
                .iter()
                .map(|node| Replica::healthy(*node, 100, Checksum::sha256([3_u8; 32]), now))
                .collect(),
            now,
        )
    }

    fn topology(nodes: Vec<NodeRecord>) -> ClusterTopology {
        let config = ClusterConfig {
            capacity_safety_margin_bytes: 0,
            unknown_upload_size_reservation_bytes: 1,
            ..ClusterConfig::default()
        };
        ClusterTopology::new(ClusterId::new(), config, nodes)
    }

    #[test]
    fn a_new_empty_node_receives_data_from_loaded_nodes() {
        let loaded_first = node("a", 1_000, 100);
        let loaded_second = node("b", 1_000, 120);
        let fresh = node("c", 1_000, 1_000);
        let fresh_id = fresh.node_id;
        let topology = topology(vec![loaded_first.clone(), loaded_second.clone(), fresh]);
        let candidates: Vec<_> = (0..4)
            .map(|_| RebalanceCandidate {
                placement: placement(&[loaded_first.node_id, loaded_second.node_id], 2),
            })
            .collect();
        let moves = plan_rebalance(
            &topology,
            &candidates,
            &CapacityAwarePlacement::new(None),
            10,
        );
        assert!(!moves.is_empty(), "an empty node must attract replicas");
        for movement in &moves {
            assert_eq!(movement.target_node, fresh_id);
            assert_ne!(movement.source_node, fresh_id);
        }
    }

    #[test]
    fn a_balanced_cluster_plans_no_movement() {
        let topology = topology(vec![
            node("a", 1_000, 500),
            node("b", 1_000, 500),
            node("c", 1_000, 500),
        ]);
        let moves = plan_rebalance(&topology, &[], &CapacityAwarePlacement::new(None), 10);
        assert!(moves.is_empty());
    }

    #[test]
    fn decommission_is_refused_when_it_would_break_durability() {
        let first = node("a", 1_000, 500);
        let second = node("b", 1_000, 500);
        let topology = topology(vec![first.clone(), second.clone()]);
        let single = placement(&[first.node_id], 3);
        let replicated = placement(&[first.node_id, second.node_id], 3);

        let unsafe_check =
            DecommissionSafety::evaluate(&topology, first.node_id, std::slice::from_ref(&single));
        assert!(!unsafe_check.safe);
        assert_eq!(unsafe_check.unavailable_payloads, 1);
        assert!(unsafe_check.reason.contains("unreadable"));

        let still_unsafe = DecommissionSafety::evaluate(
            &topology,
            first.node_id,
            std::slice::from_ref(&replicated),
        );
        assert!(
            !still_unsafe.safe,
            "dropping below the required acknowledgement count is not safe"
        );
        assert_eq!(still_unsafe.at_risk_payloads, 1);
    }

    #[test]
    fn decommission_is_allowed_when_durability_survives() {
        let first = node("a", 1_000, 500);
        let second = node("b", 1_000, 500);
        let third = node("c", 1_000, 500);
        let topology = topology(vec![first.clone(), second.clone(), third.clone()]);
        let replicated = placement(&[first.node_id, second.node_id, third.node_id], 3);
        let check = DecommissionSafety::evaluate(&topology, first.node_id, &[replicated]);
        assert!(check.safe, "{}", check.reason);
        assert_eq!(check.replicas_remaining, 1);
        assert_eq!(check.bytes_remaining, 100);
    }

    #[test]
    fn a_node_holding_nothing_is_always_safe_to_remove() {
        let first = node("a", 1_000, 500);
        let second = node("b", 1_000, 500);
        let topology = topology(vec![first.clone(), second.clone()]);
        let check = DecommissionSafety::evaluate(
            &topology,
            second.node_id,
            &[placement(&[first.node_id], 1)],
        );
        assert!(check.safe);
        assert_eq!(check.replicas_remaining, 0);
    }
}
