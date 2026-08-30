//! Rebalance planning and destructive-operation safety checks.
//!
//! Both problems are pure functions over a topology view and a set of placement
//! records, which is what makes them testable without a running cluster.

use std::collections::{BTreeMap, BTreeSet};

use record_store_core::{DeviceId, NodeId, ObjectId};
use serde::{Deserialize, Serialize};

use crate::{
    config::CapacityLevel,
    device::DeviceRecord,
    placement::{ObjectPlacementRequest, PlacementPolicy},
    replica::PayloadPlacement,
    topology::{ClusterTopology, NodeRecord, NodeState},
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
/// Balance is measured per device, not per node. A node holding one full drive
/// and three empty ones is not balanced, and a node-level view cannot see the
/// difference: it reports a comfortable average while writes to the class backed
/// by the full drive fail. Devices are what placement selects, so devices are
/// what rebalancing has to even out.
///
/// A move preserves the replica count. The source replica is described to
/// placement as absent and its device excluded, so the engine chooses one
/// replacement under the same failure-domain rules that governed the original
/// decision. That also makes a drive-to-drive move within one node fall out
/// naturally: the node still ends up holding exactly one copy.
///
/// A source replica is never removed here; that happens only after the
/// destination is written, verified, and committed.
#[must_use]
pub fn plan_rebalance(
    topology: &ClusterTopology,
    candidates: &[RebalanceCandidate],
    policy: &dyn PlacementPolicy,
    maximum_moves: usize,
) -> Vec<RebalanceMove> {
    if maximum_moves == 0 {
        return Vec::new();
    }

    // Only devices on participating nodes can send or receive.
    let devices: Vec<(&NodeRecord, &DeviceRecord)> = topology
        .nodes
        .iter()
        .filter(|node| node.state == NodeState::Healthy)
        .flat_map(|node| node.devices.iter().map(move |device| (node, device)))
        .collect();
    if devices.len() < 2 {
        return Vec::new();
    }

    let total: u64 = devices
        .iter()
        .map(|(_, device)| u64::from(device.capacity.utilization_permille() / 10))
        .sum();
    let mean = total / u64::try_from(devices.len()).unwrap_or(1).max(1);
    let tolerance = u64::from(topology.config.rebalance.tolerance_percent);

    let mut donors: BTreeSet<DeviceId> = BTreeSet::new();
    let mut recipients: BTreeSet<DeviceId> = BTreeSet::new();
    for (_, device) in &devices {
        let utilization = u64::from(device.capacity.utilization_permille() / 10);
        let level = topology
            .config
            .watermarks
            .level(device.capacity.utilization_permille() / 10);
        if utilization > mean.saturating_add(tolerance) || level.needs_relief() {
            donors.insert(device.id);
        }
        // A device that cannot take new placement cannot take a rebalance
        // either, so draining and failed drives are never destinations.
        if utilization.saturating_add(tolerance) < mean
            && level == CapacityLevel::Normal
            && device.state.accepts_new_placements()
        {
            recipients.insert(device.id);
        }
    }
    if donors.is_empty() || recipients.is_empty() {
        return Vec::new();
    }

    let mut moves = Vec::new();
    let mut arrivals: BTreeMap<DeviceId, usize> = BTreeMap::new();
    let fair_share = candidates.len().div_ceil(recipients.len().max(1)).max(1);

    for candidate in candidates {
        if moves.len() >= maximum_moves {
            break;
        }
        let placement = &candidate.placement;
        let Some(source) = placement
            .replicas
            .iter()
            .find(|replica| donors.contains(&replica.device_id))
        else {
            continue;
        };
        if !source.state.usable_as_source() {
            continue;
        }

        // Every other replica stays where it is, so placement sees them as
        // occupied domains. The source is deliberately omitted: its slot is the
        // one being refilled.
        let remaining: Vec<(NodeId, DeviceId)> = placement
            .replicas
            .iter()
            .filter(|replica| replica.device_id != source.device_id)
            .map(|replica| (replica.node_id, replica.device_id))
            .collect();
        let desired = u8::try_from(placement.replicas.len()).unwrap_or(u8::MAX);
        let request =
            ObjectPlacementRequest::new(
                placement.object_id,
                desired,
                1,
                placement.storage_class.clone(),
            )
            .with_size_hint(Some(placement.size))
            .with_existing_targets(remaining)
            .with_excluded_devices(devices.iter().map(|(_, device)| device.id).filter(
                |device_id| {
                    *device_id == source.device_id
                        || !recipients.contains(device_id)
                        || arrivals.get(device_id).copied().unwrap_or(0) >= fair_share
                },
            ));
        let Ok(plan) = policy.place(&request, topology) else {
            continue;
        };
        let Some(target) = plan.targets.first() else {
            continue;
        };
        *arrivals.entry(target.device_id).or_insert(0) += 1;
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

    /// Builds a device with an explicit utilization.
    fn drive(node: NodeId, usable: u64, available: u64) -> DeviceRecord {
        let mut device = DeviceRecord::legacy_directory(
            node,
            None,
            StorageClass::default(),
            crate::device::DeviceCapacity {
                raw_bytes: usable,
                usable_bytes: usable,
                allocated_bytes: usable.saturating_sub(available),
                reserved_bytes: 0,
                available_bytes: available,
            },
        );
        device.id = DeviceId::new();
        device
    }

    /// One full drive on an otherwise-healthy node has to be relieved.
    ///
    /// This is the case a node-level view cannot see: the node's average looks
    /// comfortable while the class backed by the full drive can take no more
    /// writes. Devices are what placement selects, so devices are what has to be
    /// evened out.
    #[test]
    fn a_full_drive_is_relieved_even_when_its_node_looks_balanced() {
        let mut node = node("a", 4_000, 3_000);
        let full = drive(node.node_id, 1_000, 20);
        let full_id = full.id;
        node.devices = vec![
            full,
            drive(node.node_id, 1_000, 990),
            drive(node.node_id, 1_000, 990),
            drive(node.node_id, 1_000, 990),
        ];
        let node_id = node.node_id;

        let mut topology = ClusterTopology::new(
            ClusterId::new(),
            ClusterConfig {
                capacity_safety_margin_bytes: 0,
                unknown_upload_size_reservation_bytes: 1,
                ..ClusterConfig::default()
            },
            vec![node],
        );
        // Spreading within one machine is what this cluster can do.
        topology.config.failure_domain_scope = crate::topology::FailureDomainScope::Device;

        let mut record = placement(&[node_id], 1);
        record.replicas[0].device_id = full_id;
        let candidates = vec![RebalanceCandidate { placement: record }];

        let moves = plan_rebalance(
            &topology,
            &candidates,
            &CapacityAwarePlacement::new(None),
            8,
        );

        assert_eq!(moves.len(), 1, "the full drive should be relieved");
        assert_eq!(moves[0].source_device, full_id);
        assert_ne!(
            moves[0].target_device, full_id,
            "a move onto the same drive relieves nothing"
        );
        assert_eq!(
            moves[0].target_node, node_id,
            "the only node available is the one it is already on"
        );
    }

    /// A drive that cannot take new placement must not be a destination, or a
    /// rebalance would fill a drive somebody is trying to empty.
    #[test]
    fn a_draining_drive_never_receives_a_rebalance() {
        let mut node = node("a", 2_000, 1_000);
        let full = drive(node.node_id, 1_000, 20);
        let full_id = full.id;
        let mut draining = drive(node.node_id, 1_000, 990);
        draining.state = crate::device::DeviceState::Draining;
        node.devices = vec![full, draining];
        let node_id = node.node_id;

        let mut topology = ClusterTopology::new(
            ClusterId::new(),
            ClusterConfig {
                capacity_safety_margin_bytes: 0,
                unknown_upload_size_reservation_bytes: 1,
                ..ClusterConfig::default()
            },
            vec![node],
        );
        topology.config.failure_domain_scope = crate::topology::FailureDomainScope::Device;

        let mut record = placement(&[node_id], 1);
        record.replicas[0].device_id = full_id;
        let candidates = vec![RebalanceCandidate { placement: record }];

        let moves = plan_rebalance(
            &topology,
            &candidates,
            &CapacityAwarePlacement::new(None),
            8,
        );
        assert!(
            moves.is_empty(),
            "the only spare drive is draining, so there is nowhere safe to move"
        );
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
