//! Replica placement.
//!
//! Placement is a single, independently testable decision function. Object
//! operations never select nodes themselves: they describe what they need and
//! receive a plan.

use std::collections::{BTreeMap, BTreeSet};

use record_store_core::{NodeId, ObjectId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    config::CapacityLevel,
    topology::{ClusterTopology, FailureDomainScope, NodeRecord, StorageClass},
};

/// What an operation needs placed.
#[derive(Debug, Clone)]
pub struct ObjectPlacementRequest {
    /// Payload the plan is for. Used for deterministic tie-breaking.
    pub object_id: ObjectId,
    /// Known payload length, when the request declares one.
    pub size_hint: Option<u64>,
    /// Number of replicas the policy wants.
    pub desired_replicas: u8,
    /// Minimum replicas that must be durable before acknowledging a write.
    pub required_acknowledgements: u8,
    /// Storage class replicas must live on.
    pub storage_class: StorageClass,
    /// Node that should be preferred when eligible, normally the ingress node.
    pub preferred_node: Option<NodeId>,
    /// Nodes that already hold a replica and must not be selected again.
    pub existing_nodes: BTreeSet<NodeId>,
    /// Nodes explicitly barred from this plan.
    pub excluded_nodes: BTreeSet<NodeId>,
}

impl ObjectPlacementRequest {
    /// Creates a request for a brand new payload.
    #[must_use]
    pub fn new(
        object_id: ObjectId,
        desired_replicas: u8,
        required_acknowledgements: u8,
        storage_class: StorageClass,
    ) -> Self {
        Self {
            object_id,
            size_hint: None,
            desired_replicas,
            required_acknowledgements,
            storage_class,
            preferred_node: None,
            existing_nodes: BTreeSet::new(),
            excluded_nodes: BTreeSet::new(),
        }
    }

    /// Sets the known payload size.
    #[must_use]
    pub const fn with_size_hint(mut self, size: Option<u64>) -> Self {
        self.size_hint = size;
        self
    }

    /// Prefers a node, normally the node that received the client request.
    #[must_use]
    pub const fn with_preferred_node(mut self, node_id: Option<NodeId>) -> Self {
        self.preferred_node = node_id;
        self
    }

    /// Declares replicas that already exist.
    #[must_use]
    pub fn with_existing_nodes(mut self, nodes: impl IntoIterator<Item = NodeId>) -> Self {
        self.existing_nodes = nodes.into_iter().collect();
        self
    }

    /// Declares nodes that must not be selected.
    #[must_use]
    pub fn with_excluded_nodes(mut self, nodes: impl IntoIterator<Item = NodeId>) -> Self {
        self.excluded_nodes = nodes.into_iter().collect();
        self
    }
}

/// One selected placement target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementTarget {
    /// Selected node.
    pub node_id: NodeId,
    /// Internal RPC address of that node.
    pub rpc_address: String,
    /// Failure domain the node belongs to under the active scope.
    pub domain: String,
    /// Whether this target is the node building the plan.
    pub local: bool,
}

/// A complete placement decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementPlan {
    /// Selected targets in write order, local target first when present.
    pub targets: Vec<PlacementTarget>,
    /// Replicas that must be durable before the write is acknowledged.
    pub required_acknowledgements: u8,
    /// Replicas the policy wanted.
    pub desired_replicas: u8,
    /// Whether the plan reuses a failure domain because none were left.
    pub failure_domains_reused: bool,
}

impl PlacementPlan {
    /// Returns whether the plan reaches the desired replica count.
    #[must_use]
    pub fn fully_replicated(&self) -> bool {
        self.targets.len() >= usize::from(self.desired_replicas)
    }

    /// Returns the node identifiers in plan order.
    #[must_use]
    pub fn node_ids(&self) -> Vec<NodeId> {
        self.targets.iter().map(|target| target.node_id).collect()
    }
}

/// Reasons placement cannot produce a usable plan.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PlacementError {
    /// The requested replication factor is not usable.
    #[error("invalid replication request: {0}")]
    InvalidRequest(String),
    /// No node satisfies the storage class and health requirements.
    #[error(
        "no eligible storage node for class '{storage_class}': \
         {healthy} healthy, {class_matches} matching class, {with_capacity} with capacity"
    )]
    NoEligibleNodes {
        /// Requested storage class.
        storage_class: String,
        /// Nodes in the healthy state.
        healthy: usize,
        /// Healthy nodes matching the storage class.
        class_matches: usize,
        /// Class-matching nodes with capacity headroom.
        with_capacity: usize,
    },
    /// Fewer eligible nodes than the write policy requires.
    #[error(
        "cluster cannot satisfy the durability requirement: {required} acknowledgements needed, \
         {eligible} eligible nodes available"
    )]
    InsufficientDurability {
        /// Acknowledgements the policy requires.
        required: u8,
        /// Nodes that could have been selected.
        eligible: usize,
    },
    /// Strict failure-domain placement could not be satisfied.
    #[error(
        "strict failure-domain placement across '{scope}' needs {required} distinct domains, \
         {available} are available"
    )]
    InsufficientFailureDomains {
        /// Scope replicas must be spread across.
        scope: String,
        /// Domains required.
        required: u8,
        /// Domains available.
        available: usize,
    },
}

/// The placement decision boundary.
pub trait PlacementPolicy: Send + Sync {
    /// Selects replica targets for one payload.
    fn place(
        &self,
        request: &ObjectPlacementRequest,
        topology: &ClusterTopology,
    ) -> Result<PlacementPlan, PlacementError>;
}

/// Capacity- and failure-domain-aware placement.
///
/// The algorithm is intentionally simple and deterministic:
///
/// 1. filter nodes by state, storage class, exclusions, and capacity headroom;
/// 2. prefer a fresh failure domain for every additional replica;
/// 3. inside a domain, prefer the least-utilized node;
/// 4. break remaining ties with a stable hash of the payload and node, so every
///    node in the cluster computes the same plan for the same inputs.
#[derive(Debug, Clone, Copy, Default)]
pub struct CapacityAwarePlacement {
    /// Node building the plan, preferred so writes can stay local.
    local_node: Option<NodeId>,
}

impl CapacityAwarePlacement {
    /// Creates a policy that knows which node it runs on.
    #[must_use]
    pub const fn new(local_node: Option<NodeId>) -> Self {
        Self { local_node }
    }
}

#[derive(Debug, Clone)]
struct Candidate {
    node_id: NodeId,
    rpc_address: String,
    domain: String,
    utilization_permille: u32,
    preferred: bool,
    tiebreak: [u8; 32],
}

impl PlacementPolicy for CapacityAwarePlacement {
    fn place(
        &self,
        request: &ObjectPlacementRequest,
        topology: &ClusterTopology,
    ) -> Result<PlacementPlan, PlacementError> {
        if request.desired_replicas == 0 {
            return Err(PlacementError::InvalidRequest(
                "desired replica count must be at least one".into(),
            ));
        }
        if request.required_acknowledgements == 0
            || request.required_acknowledgements > request.desired_replicas
        {
            return Err(PlacementError::InvalidRequest(
                "required acknowledgements must be between one and the desired replica count"
                    .into(),
            ));
        }

        let scope = topology.config.failure_domain_scope;
        let reservation = request
            .size_hint
            .unwrap_or(topology.config.unknown_upload_size_reservation_bytes);
        let margin = topology.config.capacity_safety_margin_bytes;

        let mut healthy = 0_usize;
        let mut class_matches = 0_usize;
        let mut with_capacity = 0_usize;
        let mut candidates = Vec::new();
        for node in &topology.nodes {
            if !node.state.accepts_new_replicas() {
                continue;
            }
            healthy += 1;
            if node.storage_class != request.storage_class {
                continue;
            }
            class_matches += 1;
            if node.capacity_level(&topology.config) == CapacityLevel::Critical {
                continue;
            }
            if !node.eligible_for_placement(&topology.config) {
                continue;
            }
            if node.capacity.available_bytes < reservation.saturating_add(margin) {
                continue;
            }
            with_capacity += 1;
            if request.excluded_nodes.contains(&node.node_id)
                || request.existing_nodes.contains(&node.node_id)
            {
                continue;
            }
            candidates.push(candidate(node, request, scope, self.local_node));
        }

        if candidates.is_empty() {
            return Err(PlacementError::NoEligibleNodes {
                storage_class: request.storage_class.to_string(),
                healthy,
                class_matches,
                with_capacity,
            });
        }

        // Deterministic ordering: preferred node first, then least utilized,
        // then a stable hash so unrelated nodes still agree on the same plan.
        candidates.sort_by(|left, right| {
            right
                .preferred
                .cmp(&left.preferred)
                .then(left.utilization_permille.cmp(&right.utilization_permille))
                .then(left.tiebreak.cmp(&right.tiebreak))
                .then(left.node_id.cmp(&right.node_id))
        });

        let mut occupied: BTreeMap<String, u32> = BTreeMap::new();
        for node_id in &request.existing_nodes {
            if let Some(node) = topology.node(*node_id) {
                *occupied.entry(node.domain_key(scope)).or_insert(0) += 1;
            }
        }

        let wanted =
            usize::from(request.desired_replicas).saturating_sub(request.existing_nodes.len());
        let mut targets: Vec<PlacementTarget> = Vec::with_capacity(wanted);
        let mut reused = false;

        // First pass keeps one replica per failure domain.
        for candidate in &candidates {
            if targets.len() >= wanted {
                break;
            }
            if occupied.contains_key(&candidate.domain) {
                continue;
            }
            occupied.insert(candidate.domain.clone(), 1);
            targets.push(candidate.clone().into_target(self.local_node));
        }

        let distinct_domains = candidates
            .iter()
            .map(|candidate| candidate.domain.as_str())
            .collect::<BTreeSet<_>>()
            .len();

        if targets.len() < wanted {
            if topology.config.strict_failure_domains {
                return Err(PlacementError::InsufficientFailureDomains {
                    scope: scope.to_string(),
                    required: request.desired_replicas,
                    available: distinct_domains + request.existing_nodes.len(),
                });
            }
            // Second pass reuses domains rather than losing durability entirely.
            let selected: BTreeSet<NodeId> = targets.iter().map(|target| target.node_id).collect();
            for candidate in &candidates {
                if targets.len() >= wanted {
                    break;
                }
                if selected.contains(&candidate.node_id) {
                    continue;
                }
                reused = true;
                targets.push(candidate.clone().into_target(self.local_node));
            }
        }

        let total = targets.len() + request.existing_nodes.len();
        if total < usize::from(request.required_acknowledgements) {
            return Err(PlacementError::InsufficientDurability {
                required: request.required_acknowledgements,
                eligible: total,
            });
        }

        // Keep the local target first so the ingress node writes without a hop.
        targets.sort_by_key(|target| std::cmp::Reverse(target.local));

        Ok(PlacementPlan {
            targets,
            required_acknowledgements: request.required_acknowledgements,
            desired_replicas: request.desired_replicas,
            failure_domains_reused: reused,
        })
    }
}

impl Candidate {
    fn into_target(self, local_node: Option<NodeId>) -> PlacementTarget {
        PlacementTarget {
            local: local_node == Some(self.node_id),
            node_id: self.node_id,
            rpc_address: self.rpc_address,
            domain: self.domain,
        }
    }
}

fn candidate(
    node: &NodeRecord,
    request: &ObjectPlacementRequest,
    scope: FailureDomainScope,
    local_node: Option<NodeId>,
) -> Candidate {
    let preferred = request
        .preferred_node
        .or(local_node)
        .is_some_and(|preferred| preferred == node.node_id);
    let mut hasher = Sha256::new();
    hasher.update(request.object_id.as_uuid().as_bytes());
    hasher.update(node.node_id.as_uuid().as_bytes());
    Candidate {
        node_id: node.node_id,
        rpc_address: node.rpc_address.clone(),
        domain: node.domain_key(scope),
        utilization_permille: node.capacity.utilization_permille(),
        preferred,
        tiebreak: hasher.finalize().into(),
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use proptest::prelude::*;
    use record_store_core::ClusterId;

    use super::*;
    use crate::{
        config::ClusterConfig,
        topology::{FailureDomain, NodeActivity, NodeCapacity, NodeRecord, NodeState},
        version::ProtocolVersion,
    };

    fn record(state: NodeState, rack: &str, available: u64, total: u64) -> NodeRecord {
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
            state,
            metadata_voter: false,
            capacity: NodeCapacity {
                total_bytes: total,
                available_bytes: available,
                replica_bytes: total.saturating_sub(available),
                temporary_bytes: 0,
            },
            activity: NodeActivity::default(),
            joined_at: Utc::now(),
            started_at: Utc::now(),
            last_heartbeat_at: Some(Utc::now()),
            state_changed_at: Utc::now(),
            state_reason: None,
        }
    }

    fn topology(nodes: Vec<NodeRecord>) -> ClusterTopology {
        let config = ClusterConfig {
            capacity_safety_margin_bytes: 0,
            unknown_upload_size_reservation_bytes: 1,
            ..ClusterConfig::default()
        };
        ClusterTopology::new(ClusterId::new(), config, nodes)
    }

    fn request(replicas: u8, acks: u8) -> ObjectPlacementRequest {
        ObjectPlacementRequest::new(ObjectId::new(), replicas, acks, StorageClass::default())
            .with_size_hint(Some(10))
    }

    #[test]
    fn distinct_failure_domains_are_preferred() {
        let nodes = vec![
            record(NodeState::Healthy, "a", 1_000, 1_000),
            record(NodeState::Healthy, "a", 1_000, 1_000),
            record(NodeState::Healthy, "b", 1_000, 1_000),
            record(NodeState::Healthy, "c", 1_000, 1_000),
        ];
        let topology = topology(nodes);
        let plan = CapacityAwarePlacement::new(None)
            .place(&request(3, 2), &topology)
            .expect("plan");
        let domains: BTreeSet<_> = plan.targets.iter().map(|t| t.domain.clone()).collect();
        assert_eq!(plan.targets.len(), 3);
        assert_eq!(domains.len(), 3, "each replica needs its own rack");
        assert!(!plan.failure_domains_reused);
    }

    #[test]
    fn domains_are_reused_only_when_unavoidable() {
        let nodes = vec![
            record(NodeState::Healthy, "a", 1_000, 1_000),
            record(NodeState::Healthy, "a", 1_000, 1_000),
            record(NodeState::Healthy, "a", 1_000, 1_000),
        ];
        let topology = topology(nodes);
        let plan = CapacityAwarePlacement::new(None)
            .place(&request(3, 2), &topology)
            .expect("plan");
        assert_eq!(plan.targets.len(), 3);
        assert!(plan.failure_domains_reused);
        let unique: BTreeSet<_> = plan.node_ids().into_iter().collect();
        assert_eq!(
            unique.len(),
            3,
            "replicas must still land on distinct nodes"
        );
    }

    #[test]
    fn strict_failure_domains_refuse_rather_than_reuse() {
        let nodes = vec![
            record(NodeState::Healthy, "a", 1_000, 1_000),
            record(NodeState::Healthy, "a", 1_000, 1_000),
            record(NodeState::Healthy, "a", 1_000, 1_000),
        ];
        let mut topology = topology(nodes);
        topology.config.strict_failure_domains = true;
        let error = CapacityAwarePlacement::new(None)
            .place(&request(3, 2), &topology)
            .expect_err("strict placement must refuse");
        assert!(matches!(
            error,
            PlacementError::InsufficientFailureDomains { .. }
        ));
    }

    #[test]
    fn unhealthy_and_full_nodes_are_excluded() {
        let nodes = vec![
            record(NodeState::Draining, "a", 1_000, 1_000),
            record(NodeState::Maintenance, "b", 1_000, 1_000),
            record(NodeState::Offline, "c", 1_000, 1_000),
            record(NodeState::Healthy, "d", 10, 1_000),
            record(NodeState::Healthy, "e", 1_000, 1_000),
        ];
        let topology = topology(nodes);
        let plan = CapacityAwarePlacement::new(None)
            .place(&request(1, 1), &topology)
            .expect("plan");
        assert_eq!(plan.targets.len(), 1);
        assert_eq!(plan.targets[0].domain, "rack:e");
    }

    #[test]
    fn durability_shortfall_is_an_explicit_error() {
        let topology = topology(vec![record(NodeState::Healthy, "a", 1_000, 1_000)]);
        let error = CapacityAwarePlacement::new(None)
            .place(&request(3, 2), &topology)
            .expect_err("must refuse when quorum is impossible");
        assert!(matches!(
            error,
            PlacementError::InsufficientDurability {
                required: 2,
                eligible: 1
            }
        ));
    }

    #[test]
    fn degraded_plans_still_satisfy_required_acknowledgements() {
        let topology = topology(vec![
            record(NodeState::Healthy, "a", 1_000, 1_000),
            record(NodeState::Healthy, "b", 1_000, 1_000),
        ]);
        let plan = CapacityAwarePlacement::new(None)
            .place(&request(3, 2), &topology)
            .expect("plan");
        assert_eq!(plan.targets.len(), 2);
        assert!(!plan.fully_replicated());
        assert_eq!(plan.required_acknowledgements, 2);
    }

    #[test]
    fn local_node_is_preferred_and_ordered_first() {
        let nodes = vec![
            record(NodeState::Healthy, "a", 1_000, 1_000),
            record(NodeState::Healthy, "b", 1_000, 1_000),
            record(NodeState::Healthy, "c", 1_000, 1_000),
        ];
        let local = nodes[2].node_id;
        let topology = topology(nodes);
        let plan = CapacityAwarePlacement::new(Some(local))
            .place(&request(2, 2), &topology)
            .expect("plan");
        assert_eq!(plan.targets[0].node_id, local);
        assert!(plan.targets[0].local);
    }

    #[test]
    fn existing_replicas_are_never_selected_twice() {
        let nodes = vec![
            record(NodeState::Healthy, "a", 1_000, 1_000),
            record(NodeState::Healthy, "b", 1_000, 1_000),
            record(NodeState::Healthy, "c", 1_000, 1_000),
        ];
        let existing = nodes[0].node_id;
        let topology = topology(nodes);
        let plan = CapacityAwarePlacement::new(None)
            .place(&request(3, 1).with_existing_nodes([existing]), &topology)
            .expect("plan");
        assert_eq!(plan.targets.len(), 2);
        assert!(!plan.node_ids().contains(&existing));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        #[test]
        fn plans_never_duplicate_a_node_and_respect_the_replica_count(
            racks in prop::collection::vec(0_u8..4, 1..12),
            desired in 1_u8..4,
        ) {
            let nodes: Vec<_> = racks
                .iter()
                .map(|rack| record(NodeState::Healthy, &format!("r{rack}"), 1_000, 1_000))
                .collect();
            let available = nodes.len();
            let topology = topology(nodes);
            let acks = 1;
            let outcome = CapacityAwarePlacement::new(None)
                .place(&request(desired, acks), &topology);
            match outcome {
                Ok(plan) => {
                    let unique: BTreeSet<_> = plan.node_ids().into_iter().collect();
                    prop_assert_eq!(unique.len(), plan.targets.len());
                    prop_assert!(plan.targets.len() <= usize::from(desired));
                    prop_assert!(plan.targets.len() >= usize::from(acks));
                    prop_assert!(plan.targets.len() <= available);
                }
                Err(error) => {
                    let expected = matches!(
                        error,
                        PlacementError::InsufficientDurability { .. }
                            | PlacementError::NoEligibleNodes { .. }
                    );
                    prop_assert!(expected, "unexpected placement error");
                }
            }
        }

        #[test]
        fn placement_is_deterministic_for_identical_inputs(
            racks in prop::collection::vec(0_u8..5, 3..10),
        ) {
            let nodes: Vec<_> = racks
                .iter()
                .map(|rack| record(NodeState::Healthy, &format!("r{rack}"), 1_000, 1_000))
                .collect();
            let topology = topology(nodes);
            let request = request(3, 1);
            let policy = CapacityAwarePlacement::new(None);
            let first = policy.place(&request, &topology);
            let second = policy.place(&request, &topology);
            prop_assert_eq!(first.ok().map(|p| p.node_ids()), second.ok().map(|p| p.node_ids()));
        }
    }
}
