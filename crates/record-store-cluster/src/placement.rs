//! Replica placement.
//!
//! Placement is a single, independently testable decision function. Object
//! operations never select nodes themselves: they describe what they need and
//! receive a plan.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::{self, Display, Formatter},
};

use record_store_core::{DeviceId, NodeId, ObjectId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    config::CapacityLevel,
    device::{DeviceKind, DeviceRecord},
    policy::StoragePolicy,
    topology::{ClusterMapEpoch, ClusterTopology, FailureDomainScope, NodeRecord, StorageClass},
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
    /// Existing `(node, device)` replicas that count toward the desired total.
    pub existing_targets: BTreeSet<(NodeId, DeviceId)>,
    /// Explicitly barred devices.
    pub excluded_devices: BTreeSet<DeviceId>,
    /// Resolved policy for the requested class.
    ///
    /// `None` keeps the cluster-wide configuration, which is what standalone and
    /// pre-storage-class deployments get.
    pub policy: Option<StoragePolicy>,
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
            existing_targets: BTreeSet::new(),
            excluded_devices: BTreeSet::new(),
            policy: None,
        }
    }

    /// Applies a resolved storage policy to the request.
    ///
    /// The policy decides the failure domain, the device kinds, and the free
    /// space to hold back. It does not change the replica count already asked
    /// for: the caller resolved that from the same policy and may have adjusted
    /// it for an existing object.
    #[must_use]
    pub fn with_policy(mut self, policy: Option<StoragePolicy>) -> Self {
        if let Some(policy) = &policy {
            self.storage_class = policy.class.clone();
        }
        self.policy = policy;
        self
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

    /// Declares replicas that already exist on exact devices.
    #[must_use]
    pub fn with_existing_targets(
        mut self,
        targets: impl IntoIterator<Item = (NodeId, DeviceId)>,
    ) -> Self {
        self.existing_targets = targets.into_iter().collect();
        self
    }

    /// Declares devices that must not be selected.
    #[must_use]
    pub fn with_excluded_devices(mut self, devices: impl IntoIterator<Item = DeviceId>) -> Self {
        self.excluded_devices = devices.into_iter().collect();
        self
    }
}

/// One selected placement target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementTarget {
    /// Selected node.
    pub node_id: NodeId,
    /// Selected independently managed device.
    pub device_id: DeviceId,
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
    /// Committed cluster-map generation used for the decision.
    pub epoch: ClusterMapEpoch,
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
    NoEligibleDevices {
        /// Requested storage class.
        storage_class: String,
        /// Nodes in the healthy state.
        healthy: usize,
        /// Active devices matching the storage class.
        class_matches: usize,
        /// Class-matching devices with capacity headroom.
        with_capacity: usize,
    },
    /// Fewer eligible nodes than the write policy requires.
    #[error(
        "cluster cannot satisfy the durability requirement: {required} acknowledgements needed, \
         {eligible} eligible devices available"
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
    device_id: DeviceId,
    rpc_address: String,
    domain: String,
    score: f64,
    effective_weight: u128,
    kind: DeviceKind,
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

        let policy = request.policy.as_ref();
        let scope = policy.map_or(topology.config.failure_domain_scope, |policy| {
            policy.failure_domain
        });
        let strict = policy.map_or(topology.config.strict_failure_domains, |policy| {
            policy.strict_failure_domains
        });
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
            for device in &node.devices {
                if !eligible_class(request, device) || !device.state.accepts_new_placements() {
                    continue;
                }
                class_matches += 1;
                let level = topology
                    .config
                    .watermarks
                    .level(device.capacity.utilization_permille() / 10);
                // A policy reserve is held back on top of the cluster margin, so
                // a class can keep headroom the rest of the cluster does not.
                let reserve = margin.saturating_add(policy.map_or(0, |policy| {
                    policy.reserved_bytes(device.capacity.usable_bytes)
                }));
                if level == CapacityLevel::Critical
                    || !level.accepts_new_replicas()
                    || !device.eligible_for_placement(reservation, reserve)
                {
                    continue;
                }
                with_capacity += 1;
                if request.excluded_nodes.contains(&node.node_id)
                    || request.existing_nodes.contains(&node.node_id)
                    || request.excluded_devices.contains(&device.id)
                    || request
                        .existing_targets
                        .contains(&(node.node_id, device.id))
                {
                    continue;
                }
                candidates.push(candidate(node, device, request, scope));
            }
        }

        if candidates.is_empty() {
            return Err(PlacementError::NoEligibleDevices {
                storage_class: request.storage_class.to_string(),
                healthy,
                class_matches,
                with_capacity,
            });
        }

        // Weighted rendezvous ordering. Lower exponential-race scores win.
        // Every input is committed cluster state or request identity; ingress
        // locality and process-local iteration order never change the target set.
        candidates.sort_by(|left, right| {
            left.score
                .total_cmp(&right.score)
                .then(left.node_id.cmp(&right.node_id))
                .then(left.device_id.cmp(&right.device_id))
        });

        let mut occupied: BTreeMap<String, u32> = BTreeMap::new();
        for (node_id, device_id) in &request.existing_targets {
            if let Some(node) = topology.node(*node_id) {
                let domain = placement_domain(node, *device_id, scope);
                *occupied.entry(domain).or_insert(0) += 1;
            }
        }
        for node_id in &request.existing_nodes {
            if let Some(node) = topology.node(*node_id) {
                *occupied.entry(node.domain_key(scope)).or_insert(0) += 1;
            }
        }

        let existing_count = request
            .existing_targets
            .len()
            .max(request.existing_nodes.len());
        let wanted = usize::from(request.desired_replicas).saturating_sub(existing_count);
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
            if strict {
                return Err(PlacementError::InsufficientFailureDomains {
                    scope: scope.to_string(),
                    required: request.desired_replicas,
                    available: distinct_domains + occupied.len(),
                });
            }
            // Second pass reuses domains rather than losing durability entirely.
            let selected: BTreeSet<DeviceId> =
                targets.iter().map(|target| target.device_id).collect();
            for candidate in &candidates {
                if targets.len() >= wanted {
                    break;
                }
                if selected.contains(&candidate.device_id) {
                    continue;
                }
                reused = true;
                targets.push(candidate.clone().into_target(self.local_node));
            }
        }

        // Selection is by rendezvous score alone, so every node computes the same
        // target set. Ordering is a local concern: putting the ingress node's own
        // device first saves a network round trip on the first replica without
        // changing which devices were chosen.
        if let Some(local) = self.local_node
            && let Some(position) = targets.iter().position(|target| target.node_id == local)
        {
            targets[..=position].rotate_right(1);
        }

        let total = targets.len() + existing_count;
        if total < usize::from(request.required_acknowledgements) {
            return Err(PlacementError::InsufficientDurability {
                required: request.required_acknowledgements,
                eligible: total,
            });
        }

        Ok(PlacementPlan {
            epoch: topology.epoch,
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
            device_id: self.device_id,
            rpc_address: self.rpc_address,
            domain: self.domain,
        }
    }
}

fn candidate(
    node: &NodeRecord,
    device: &DeviceRecord,
    request: &ObjectPlacementRequest,
    scope: FailureDomainScope,
) -> Candidate {
    // The epoch is deliberately not hashed. Rendezvous hashing earns its keep by
    // moving only the keys that belong on a changed device, and the epoch
    // advances on every registration, so mixing it in would re-roll every score
    // and turn each expansion into a full data migration.
    let mut hasher = Sha256::new();
    hasher.update(request.object_id.as_uuid().as_bytes());
    hasher.update(request.storage_class.as_str().as_bytes());
    hasher.update(node.node_id.as_uuid().as_bytes());
    hasher.update(device.id.as_uuid().as_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    let sample = u64::from_be_bytes(digest[..8].try_into().unwrap_or([0; 8]));
    let unit = (sample as f64 + 1.0) / (u64::MAX as f64 + 1.0);
    let effective_weight = device.effective_weight().max(1);
    Candidate {
        node_id: node.node_id,
        device_id: device.id,
        rpc_address: node.rpc_address.clone(),
        domain: placement_domain(node, device.id, scope),
        score: -unit.ln() / effective_weight as f64,
        effective_weight,
        kind: device.kind,
    }
}

/// Returns whether a device satisfies the request's class and hardware policy.
///
/// The class label always has to match. A policy narrows further by device kind,
/// which is how `hot` comes to mean "solid state only" without the placement
/// engine knowing what any of those words mean.
fn eligible_class(request: &ObjectPlacementRequest, device: &DeviceRecord) -> bool {
    match &request.policy {
        Some(policy) => policy.accepts_device(device),
        None => device.storage_class == request.storage_class,
    }
}

fn placement_domain(node: &NodeRecord, device_id: DeviceId, scope: FailureDomainScope) -> String {
    if scope == FailureDomainScope::Device {
        format!("device:{device_id}")
    } else {
        node.domain_key(scope)
    }
}

/// Why a device was not available to a placement decision.
///
/// An explanation that only lists winners answers the easy question. The useful
/// one is why the device an operator expected was passed over, so every
/// rejection carries the rule that rejected it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementRejection {
    /// The node is not accepting new replicas.
    NodeNotAcceptingReplicas,
    /// The device's lifecycle state bars new placement.
    DeviceNotActive,
    /// Observed device health bars new placement.
    DeviceUnhealthy,
    /// The device belongs to a different storage class.
    WrongStorageClass,
    /// The policy's device filter excludes this hardware kind.
    DeviceKindFiltered,
    /// The device is above the cluster capacity watermark.
    AboveCapacityWatermark,
    /// Free space is below the request size plus the reserved margin.
    InsufficientFreeSpace,
    /// The request excluded this node.
    NodeExcluded,
    /// The request excluded this device.
    DeviceExcluded,
    /// A replica of this payload already lives here.
    AlreadyHoldsReplica,
}

impl Display for PlacementRejection {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NodeNotAcceptingReplicas => "node is not accepting new replicas",
            Self::DeviceNotActive => "device is not active",
            Self::DeviceUnhealthy => "device health bars placement",
            Self::WrongStorageClass => "device belongs to another storage class",
            Self::DeviceKindFiltered => "storage policy excludes this device kind",
            Self::AboveCapacityWatermark => "device is above the capacity watermark",
            Self::InsufficientFreeSpace => "not enough free space for this write plus the reserve",
            Self::NodeExcluded => "node excluded by the request",
            Self::DeviceExcluded => "device excluded by the request",
            Self::AlreadyHoldsReplica => "already holds a replica of this payload",
        })
    }
}

/// One device that could not be selected, and the rule that ruled it out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementExclusion {
    /// Node owning the device.
    pub node_id: NodeId,
    /// Device that was not selected.
    pub device_id: DeviceId,
    /// Physical kind.
    pub kind: DeviceKind,
    /// Why it was not eligible.
    pub reason: PlacementRejection,
}

/// One candidate in an operator-facing placement explanation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlacementCandidateExplanation {
    /// Candidate node.
    pub node_id: NodeId,
    /// Candidate device.
    pub device_id: DeviceId,
    /// Physical kind.
    pub kind: DeviceKind,
    /// Failure-domain key used by the request.
    pub domain: String,
    /// Stable effective capacity weight.
    pub effective_weight: u128,
    /// Weighted-rendezvous score; lower scores rank first.
    pub score: f64,
    /// Whether this candidate was selected.
    pub selected: bool,
}

/// Read-only explanation of one deterministic decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlacementExplanation {
    /// Committed map generation.
    pub epoch: ClusterMapEpoch,
    /// Requested storage class.
    pub storage_class: StorageClass,
    /// Enforced failure-domain scope.
    pub failure_domain: FailureDomainScope,
    /// Selected plan.
    pub decision: PlacementPlan,
    /// Eligible candidates in rendezvous order.
    pub candidates: Vec<PlacementCandidateExplanation>,
    /// Devices that were not eligible, and why.
    pub excluded: Vec<PlacementExclusion>,
}

impl CapacityAwarePlacement {
    /// Explains which devices were eligible, which were not, and why.
    ///
    /// The walk mirrors `place` rather than approximating it: the same filters
    /// in the same order, so an explanation cannot describe a decision the
    /// engine would not make.
    pub fn explain(
        &self,
        request: &ObjectPlacementRequest,
        topology: &ClusterTopology,
    ) -> Result<PlacementExplanation, PlacementError> {
        let decision = self.place(request, topology)?;
        let policy = request.policy.as_ref();
        let scope = policy.map_or(topology.config.failure_domain_scope, |policy| {
            policy.failure_domain
        });
        let reservation = request
            .size_hint
            .unwrap_or(topology.config.unknown_upload_size_reservation_bytes);
        let margin = topology.config.capacity_safety_margin_bytes;
        let selected: BTreeSet<DeviceId> = decision
            .targets
            .iter()
            .map(|target| target.device_id)
            .collect();

        let mut candidates = Vec::new();
        let mut excluded = Vec::new();
        for node in &topology.nodes {
            for device in &node.devices {
                let mut reject = |reason: PlacementRejection| {
                    excluded.push(PlacementExclusion {
                        node_id: node.node_id,
                        device_id: device.id,
                        kind: device.kind,
                        reason,
                    });
                };
                if !node.state.accepts_new_replicas() {
                    reject(PlacementRejection::NodeNotAcceptingReplicas);
                    continue;
                }
                if device.storage_class != request.storage_class {
                    reject(PlacementRejection::WrongStorageClass);
                    continue;
                }
                if !eligible_class(request, device) {
                    reject(PlacementRejection::DeviceKindFiltered);
                    continue;
                }
                if !device.state.accepts_new_placements() {
                    reject(PlacementRejection::DeviceNotActive);
                    continue;
                }
                if !device.health.permits_placement() {
                    reject(PlacementRejection::DeviceUnhealthy);
                    continue;
                }
                let level = topology
                    .config
                    .watermarks
                    .level(device.capacity.utilization_permille() / 10);
                if level == CapacityLevel::Critical || !level.accepts_new_replicas() {
                    reject(PlacementRejection::AboveCapacityWatermark);
                    continue;
                }
                let reserve = margin.saturating_add(policy.map_or(0, |policy| {
                    policy.reserved_bytes(device.capacity.usable_bytes)
                }));
                if !device.eligible_for_placement(reservation, reserve) {
                    reject(PlacementRejection::InsufficientFreeSpace);
                    continue;
                }
                if request.excluded_nodes.contains(&node.node_id) {
                    reject(PlacementRejection::NodeExcluded);
                    continue;
                }
                if request.excluded_devices.contains(&device.id) {
                    reject(PlacementRejection::DeviceExcluded);
                    continue;
                }
                if request.existing_nodes.contains(&node.node_id)
                    || request
                        .existing_targets
                        .contains(&(node.node_id, device.id))
                {
                    reject(PlacementRejection::AlreadyHoldsReplica);
                    continue;
                }

                let candidate = candidate(node, device, request, scope);
                candidates.push(PlacementCandidateExplanation {
                    node_id: candidate.node_id,
                    device_id: candidate.device_id,
                    kind: candidate.kind,
                    domain: candidate.domain,
                    effective_weight: candidate.effective_weight,
                    score: candidate.score,
                    selected: selected.contains(&candidate.device_id),
                });
            }
        }
        candidates.sort_by(|left, right| {
            left.score
                .total_cmp(&right.score)
                .then(left.node_id.cmp(&right.node_id))
                .then(left.device_id.cmp(&right.device_id))
        });
        excluded.sort_by_key(|entry| (entry.node_id, entry.device_id));
        Ok(PlacementExplanation {
            epoch: topology.epoch,
            storage_class: request.storage_class.clone(),
            failure_domain: scope,
            decision,
            candidates,
            excluded,
        })
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
        device::DeviceCapacity,
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
            devices: Vec::new(),
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

    /// Builds a device with an explicit capacity and class.
    fn device_with(
        node: NodeId,
        class: &StorageClass,
        capacity: u64,
        state: crate::device::DeviceState,
    ) -> DeviceRecord {
        let mut device = DeviceRecord::legacy_directory(
            node,
            Some(std::path::PathBuf::from("/srv/record-store")),
            class.clone(),
            DeviceCapacity {
                raw_bytes: capacity,
                usable_bytes: capacity,
                allocated_bytes: 0,
                reserved_bytes: 0,
                available_bytes: capacity,
            },
        );
        // Legacy identity is derived from the node, so multi-device nodes need
        // distinct identities.
        device.id = DeviceId::new();
        device.state = state;
        device
    }

    /// A node holding several drives is still one failure domain.
    ///
    /// This is the invariant that makes "3 replicas across nodes" mean anything:
    /// three drives in one chassis die together with the chassis, so spreading
    /// replicas across them is not durability, it only looks like it.
    #[test]
    fn several_drives_on_one_node_never_hold_more_than_one_replica() {
        use crate::device::DeviceState;

        let mut nodes = Vec::new();
        for index in 0..3 {
            let mut node = record(
                NodeState::Healthy,
                &format!("rack-{index}"),
                1 << 40,
                1 << 40,
            );
            let class = node.storage_class.clone();
            // Four independent drives per node, as §104 describes.
            node.devices = (0..4)
                .map(|_| device_with(node.node_id, &class, 1 << 40, DeviceState::Active))
                .collect();
            nodes.push(node);
        }
        let mut topology = topology(nodes);
        topology.config.failure_domain_scope = FailureDomainScope::Node;

        let policy = CapacityAwarePlacement::new(None);
        for index in 0..500_u128 {
            let request = ObjectPlacementRequest::new(
                ObjectId::from_uuid(uuid::Uuid::from_u128(index)),
                3,
                2,
                StorageClass::default(),
            )
            .with_size_hint(Some(10));
            let plan = policy.place(&request, &topology).expect("plan");

            let nodes_used: BTreeSet<NodeId> =
                plan.targets.iter().map(|target| target.node_id).collect();
            assert_eq!(
                nodes_used.len(),
                plan.targets.len(),
                "two replicas landed on one node: {:?}",
                plan.targets
            );
            let devices_used: BTreeSet<DeviceId> =
                plan.targets.iter().map(|target| target.device_id).collect();
            assert_eq!(
                devices_used.len(),
                plan.targets.len(),
                "one device was selected twice"
            );
        }
    }

    /// Every drive is its own placement target, so a multi-drive node really
    /// does contribute more than one place to put data.
    #[test]
    fn each_drive_is_an_independent_placement_target() {
        use crate::device::DeviceState;

        let mut node = record(NodeState::Healthy, "solo", 1 << 40, 1 << 40);
        let class = node.storage_class.clone();
        node.devices = (0..4)
            .map(|_| device_with(node.node_id, &class, 1 << 40, DeviceState::Active))
            .collect();
        let expected: BTreeSet<DeviceId> = node.devices.iter().map(|device| device.id).collect();
        let mut topology = topology(vec![node]);
        // Device scope is how an operator asks for spread within one machine.
        topology.config.failure_domain_scope = FailureDomainScope::Device;

        let policy = CapacityAwarePlacement::new(None);
        let mut seen = BTreeSet::new();
        for index in 0..200_u128 {
            let request = ObjectPlacementRequest::new(
                ObjectId::from_uuid(uuid::Uuid::from_u128(index)),
                3,
                2,
                StorageClass::default(),
            )
            .with_size_hint(Some(10));
            let plan = policy.place(&request, &topology).expect("plan");
            assert_eq!(
                plan.targets.len(),
                3,
                "three drives should satisfy three replicas"
            );
            seen.extend(plan.targets.iter().map(|target| target.device_id));
        }
        assert_eq!(
            seen, expected,
            "every drive should be reachable by placement"
        );
    }

    /// Capacity has to steer placement, or a 24 TB drive fills at the same rate
    /// as a 4 TB one and the cluster runs out of space early.
    #[test]
    fn placement_follows_device_capacity() {
        use crate::device::DeviceState;

        // The heterogeneous topology from §103, one drive per node.
        let capacities = [4, 8, 20, 24].map(|terabytes| terabytes * (1_u64 << 40));
        let nodes: Vec<NodeRecord> = capacities
            .iter()
            .enumerate()
            .map(|(index, capacity)| {
                let mut node = record(
                    NodeState::Healthy,
                    &format!("rack-{index}"),
                    *capacity,
                    *capacity,
                );
                let class = node.storage_class.clone();
                node.devices = vec![device_with(
                    node.node_id,
                    &class,
                    *capacity,
                    DeviceState::Active,
                )];
                node
            })
            .collect();
        let by_capacity: Vec<(DeviceId, u64)> = nodes
            .iter()
            .map(|node| (node.devices[0].id, node.devices[0].capacity.usable_bytes))
            .collect();
        let topology = topology(nodes);

        let policy = CapacityAwarePlacement::new(None);
        let mut counts: BTreeMap<DeviceId, usize> = BTreeMap::new();
        let samples = 4_000_u128;
        for index in 0..samples {
            let request = ObjectPlacementRequest::new(
                ObjectId::from_uuid(uuid::Uuid::from_u128(index)),
                1,
                1,
                StorageClass::default(),
            )
            .with_size_hint(Some(10));
            let plan = policy.place(&request, &topology).expect("plan");
            *counts.entry(plan.targets[0].device_id).or_insert(0) += 1;
        }

        let total_capacity: u64 = by_capacity.iter().map(|(_, capacity)| *capacity).sum();
        for (device_id, capacity) in by_capacity {
            let share = *counts.get(&device_id).unwrap_or(&0) as f64 / samples as f64;
            let expected = capacity as f64 / total_capacity as f64;
            // Statistical, not exact: weighted rendezvous is probabilistic, so
            // the assertion is a band rather than a point.
            assert!(
                (share - expected).abs() < 0.05,
                "device with {capacity} bytes took {share:.3} of placements, expected about {expected:.3}"
            );
        }
    }

    /// A drive being emptied, paused, or already lost must not be handed new
    /// data. Draining in particular would otherwise never finish.
    #[test]
    fn drives_not_in_service_receive_no_new_placement() {
        use crate::device::{DeviceHealth, DeviceState};

        for state in [
            DeviceState::Draining,
            DeviceState::Maintenance,
            DeviceState::Failed,
            DeviceState::SafeToRemove,
            DeviceState::Retired,
            DeviceState::Available,
        ] {
            let mut healthy = record(NodeState::Healthy, "a", 1 << 40, 1 << 40);
            let class = healthy.storage_class.clone();
            healthy.devices = vec![device_with(
                healthy.node_id,
                &class,
                1 << 40,
                DeviceState::Active,
            )];
            let live_device = healthy.devices[0].id;

            let mut excluded = record(NodeState::Healthy, "b", 1 << 40, 1 << 40);
            excluded.devices = vec![device_with(excluded.node_id, &class, 1 << 40, state)];
            let excluded_device = excluded.devices[0].id;

            let topology = topology(vec![healthy, excluded]);
            let policy = CapacityAwarePlacement::new(None);
            let plan = policy.place(&request(1, 1), &topology).expect("plan");
            assert_eq!(
                plan.targets[0].device_id, live_device,
                "a device in {state} must not receive new placement"
            );
            assert!(
                !plan
                    .targets
                    .iter()
                    .any(|target| target.device_id == excluded_device)
            );
        }

        // Health is a separate axis from lifecycle, and a failed drive is not a
        // placement candidate however it was administratively marked.
        let mut healthy = record(NodeState::Healthy, "a", 1 << 40, 1 << 40);
        let class = healthy.storage_class.clone();
        healthy.devices = vec![device_with(
            healthy.node_id,
            &class,
            1 << 40,
            DeviceState::Active,
        )];
        let live_device = healthy.devices[0].id;
        let mut sick = record(NodeState::Healthy, "b", 1 << 40, 1 << 40);
        sick.devices = vec![device_with(
            sick.node_id,
            &class,
            1 << 40,
            DeviceState::Active,
        )];
        sick.devices[0].health = DeviceHealth::Failed;
        let topology = topology(vec![healthy, sick]);
        let plan = CapacityAwarePlacement::new(None)
            .place(&request(1, 1), &topology)
            .expect("plan");
        assert_eq!(plan.targets[0].device_id, live_device);
    }

    /// A policy restricted to solid state must not spill onto rotational media.
    ///
    /// This is the point of separating hardware kind from storage class: the
    /// class says what the data needs, the kind says what the device is, and
    /// placement refuses to pretend one is the other.
    #[test]
    fn a_device_filter_keeps_a_class_on_the_hardware_it_asked_for() {
        use crate::device::{DeviceKind, DeviceState};
        use crate::policy::{DeviceFilter, DurabilityStrategy, StoragePolicy};

        let mut nodes = Vec::new();
        let mut solid_state = Vec::new();
        for (index, kind) in [
            DeviceKind::Nvme,
            DeviceKind::SataSsd,
            DeviceKind::SataHdd,
            DeviceKind::Unknown,
        ]
        .into_iter()
        .enumerate()
        {
            let mut node = record(
                NodeState::Healthy,
                &format!("rack-{index}"),
                1 << 40,
                1 << 40,
            );
            let class = node.storage_class.clone();
            let mut device = device_with(node.node_id, &class, 1 << 40, DeviceState::Active);
            device.kind = kind;
            if matches!(kind, DeviceKind::Nvme | DeviceKind::SataSsd) {
                solid_state.push(device.id);
            }
            node.devices = vec![device];
            nodes.push(node);
        }
        let topology = topology(nodes);

        let policy = StoragePolicy {
            class: StorageClass::default(),
            description: None,
            device_filter: DeviceFilter::allowing([DeviceKind::Nvme, DeviceKind::SataSsd])
                .expect("filter"),
            durability: DurabilityStrategy::Replication { replicas: 2 },
            failure_domain: FailureDomainScope::Node,
            strict_failure_domains: false,
            minimum_free_space_percent: 0,
        };
        policy.validate().expect("valid policy");

        let engine = CapacityAwarePlacement::new(None);
        let allowed: BTreeSet<DeviceId> = solid_state.into_iter().collect();
        for index in 0..200_u128 {
            let request = ObjectPlacementRequest::new(
                ObjectId::from_uuid(uuid::Uuid::from_u128(index)),
                2,
                2,
                StorageClass::default(),
            )
            .with_size_hint(Some(10))
            .with_policy(Some(policy.clone()));
            let plan = engine.place(&request, &topology).expect("plan");
            for target in &plan.targets {
                assert!(
                    allowed.contains(&target.device_id),
                    "a solid-state class was placed on rotational or unidentified media"
                );
            }
        }
    }

    /// A policy reserve keeps a class off a device the cluster still considers
    /// usable, so one class can hold headroom the rest of the cluster does not.
    #[test]
    fn a_policy_reserve_withholds_capacity_the_cluster_would_otherwise_use() {
        use crate::device::DeviceState;
        use crate::policy::{DeviceFilter, DurabilityStrategy, StoragePolicy};

        let mut node = record(NodeState::Healthy, "a", 1_000, 1_000);
        let class = node.storage_class.clone();
        let mut device = device_with(node.node_id, &class, 1_000, DeviceState::Active);
        // Forty percent free, which is comfortably inside the cluster's own
        // watermarks: whatever excludes this device has to be the policy.
        device.capacity.available_bytes = 400;
        node.devices = vec![device];
        let topology = topology(vec![node]);

        let mut policy = StoragePolicy {
            class: StorageClass::default(),
            description: None,
            device_filter: DeviceFilter::any(),
            durability: DurabilityStrategy::Replication { replicas: 1 },
            failure_domain: FailureDomainScope::Node,
            strict_failure_domains: false,
            minimum_free_space_percent: 0,
        };

        let request = |policy: &StoragePolicy| {
            ObjectPlacementRequest::new(ObjectId::new(), 1, 1, StorageClass::default())
                .with_size_hint(Some(10))
                .with_policy(Some(policy.clone()))
        };

        let engine = CapacityAwarePlacement::new(None);
        engine
            .place(&request(&policy), &topology)
            .expect("without a reserve the remaining space is usable");

        // Reserving half of usable capacity puts the device out of reach for
        // this class while the cluster still considers it perfectly usable.
        policy.minimum_free_space_percent = 50;
        policy.validate().expect("valid");
        let refused = engine
            .place(&request(&policy), &topology)
            .expect_err("a class must not eat into its own reserve");
        assert!(matches!(refused, PlacementError::NoEligibleDevices { .. }));
    }

    /// The policy decides the failure domain, not the cluster default, or a
    /// class asking for rack separation would silently get node separation.
    #[test]
    fn the_policy_failure_domain_overrides_the_cluster_default() {
        use crate::device::DeviceState;
        use crate::policy::{DeviceFilter, DurabilityStrategy, StoragePolicy};

        // Two racks, two nodes each: enough for node separation, not for three
        // racks.
        let mut nodes = Vec::new();
        for index in 0..4 {
            let rack = if index < 2 { "rack-a" } else { "rack-b" };
            let mut node = record(NodeState::Healthy, rack, 1 << 40, 1 << 40);
            let class = node.storage_class.clone();
            node.devices = vec![device_with(
                node.node_id,
                &class,
                1 << 40,
                DeviceState::Active,
            )];
            nodes.push(node);
        }
        let mut topology = topology(nodes);
        topology.config.failure_domain_scope = FailureDomainScope::Node;
        topology.config.strict_failure_domains = true;

        let policy = StoragePolicy {
            class: StorageClass::default(),
            description: None,
            device_filter: DeviceFilter::any(),
            durability: DurabilityStrategy::Replication { replicas: 3 },
            failure_domain: FailureDomainScope::Rack,
            strict_failure_domains: true,
            minimum_free_space_percent: 0,
        };

        let request = ObjectPlacementRequest::new(ObjectId::new(), 3, 2, StorageClass::default())
            .with_size_hint(Some(10))
            .with_policy(Some(policy));
        let error = CapacityAwarePlacement::new(None)
            .place(&request, &topology)
            .expect_err("three racks are needed and only two exist");
        assert!(
            matches!(error, PlacementError::InsufficientFailureDomains { .. }),
            "expected a failure-domain refusal, got {error}"
        );

        // The cluster default alone would have been satisfied by four nodes.
        let unrestricted =
            ObjectPlacementRequest::new(ObjectId::new(), 3, 2, StorageClass::default())
                .with_size_hint(Some(10));
        CapacityAwarePlacement::new(None)
            .place(&unrestricted, &topology)
            .expect("node separation is satisfiable");
    }

    /// An explanation has to name the rule that ruled a device out, or an
    /// operator is left guessing why the drive they expected was skipped.
    #[test]
    fn an_explanation_says_why_each_device_was_passed_over() {
        use crate::device::{DeviceHealth, DeviceKind, DeviceState};
        use crate::policy::{DeviceFilter, DurabilityStrategy, StoragePolicy};

        // One node per situation, so each rejection is unambiguous.
        let mut chosen = record(NodeState::Healthy, "a", 1 << 40, 1 << 40);
        let class = chosen.storage_class.clone();
        chosen.devices = vec![device_with(
            chosen.node_id,
            &class,
            1 << 40,
            DeviceState::Active,
        )];
        let chosen_device = chosen.devices[0].id;

        let mut draining = record(NodeState::Healthy, "b", 1 << 40, 1 << 40);
        draining.devices = vec![device_with(
            draining.node_id,
            &class,
            1 << 40,
            DeviceState::Draining,
        )];
        let draining_device = draining.devices[0].id;

        let mut sick = record(NodeState::Healthy, "c", 1 << 40, 1 << 40);
        sick.devices = vec![device_with(
            sick.node_id,
            &class,
            1 << 40,
            DeviceState::Active,
        )];
        sick.devices[0].health = DeviceHealth::Failed;
        let sick_device = sick.devices[0].id;

        let mut rotational = record(NodeState::Healthy, "d", 1 << 40, 1 << 40);
        rotational.devices = vec![device_with(
            rotational.node_id,
            &class,
            1 << 40,
            DeviceState::Active,
        )];
        rotational.devices[0].kind = DeviceKind::SataHdd;
        let rotational_device = rotational.devices[0].id;

        let mut offline = record(NodeState::Offline, "e", 1 << 40, 1 << 40);
        offline.devices = vec![device_with(
            offline.node_id,
            &class,
            1 << 40,
            DeviceState::Active,
        )];
        let offline_device = offline.devices[0].id;

        let topology = topology(vec![chosen, draining, sick, rotational, offline]);
        let policy = StoragePolicy {
            class: StorageClass::default(),
            description: None,
            device_filter: DeviceFilter::allowing([DeviceKind::FilesystemDirectory])
                .expect("filter"),
            durability: DurabilityStrategy::Replication { replicas: 1 },
            failure_domain: FailureDomainScope::Node,
            strict_failure_domains: false,
            minimum_free_space_percent: 0,
        };
        let request = ObjectPlacementRequest::new(ObjectId::new(), 1, 1, StorageClass::default())
            .with_size_hint(Some(10))
            .with_policy(Some(policy));

        let explanation = CapacityAwarePlacement::new(None)
            .explain(&request, &topology)
            .expect("explain");

        assert_eq!(explanation.decision.targets.len(), 1);
        assert_eq!(explanation.decision.targets[0].device_id, chosen_device);
        assert!(
            explanation
                .candidates
                .iter()
                .any(|candidate| candidate.device_id == chosen_device && candidate.selected)
        );

        let reason = |device: DeviceId| {
            explanation
                .excluded
                .iter()
                .find(|entry| entry.device_id == device)
                .map(|entry| entry.reason)
        };
        assert_eq!(
            reason(draining_device),
            Some(PlacementRejection::DeviceNotActive)
        );
        assert_eq!(
            reason(sick_device),
            Some(PlacementRejection::DeviceUnhealthy)
        );
        assert_eq!(
            reason(rotational_device),
            Some(PlacementRejection::DeviceKindFiltered)
        );
        assert_eq!(
            reason(offline_device),
            Some(PlacementRejection::NodeNotAcceptingReplicas)
        );
        assert!(
            reason(chosen_device).is_none(),
            "the selected device must not also be reported as excluded"
        );
    }

    /// A node with several drives really does receive proportionally more data.
    ///
    /// The drives are independent placement targets, so a machine with four of
    /// them should take about four times the share of a machine with one. This
    /// is the property that makes declaring extra drives worth doing at all, and
    /// it is not implied by any of the failure-domain rules.
    #[test]
    fn a_node_with_more_drives_takes_proportionally_more_data() {
        use crate::device::DeviceState;

        let mut wide = record(NodeState::Healthy, "rack-a", 1 << 40, 1 << 40);
        let class = wide.storage_class.clone();
        wide.devices = (0..4)
            .map(|_| device_with(wide.node_id, &class, 1 << 40, DeviceState::Active))
            .collect();
        let wide_id = wide.node_id;

        let mut narrow = record(NodeState::Healthy, "rack-b", 1 << 40, 1 << 40);
        narrow.devices = vec![device_with(
            narrow.node_id,
            &class,
            1 << 40,
            DeviceState::Active,
        )];

        let mut topology = topology(vec![wide, narrow]);
        // Device scope, so both nodes compete per drive rather than per machine.
        topology.config.failure_domain_scope = FailureDomainScope::Device;

        let policy = CapacityAwarePlacement::new(None);
        let samples = 4_000_u128;
        let mut on_wide = 0_usize;
        for index in 0..samples {
            let request = ObjectPlacementRequest::new(
                ObjectId::from_uuid(uuid::Uuid::from_u128(index)),
                1,
                1,
                StorageClass::default(),
            )
            .with_size_hint(Some(10));
            let plan = policy.place(&request, &topology).expect("plan");
            if plan.targets[0].node_id == wide_id {
                on_wide += 1;
            }
        }

        // Four drives of five, so about 80 percent. Statistical, not exact.
        let share = on_wide as f64 / samples as f64;
        assert!(
            (share - 0.8).abs() < 0.05,
            "a four-drive node took {share:.3} of placements, expected about 0.8"
        );
    }

    /// Adding a device must move only the objects that belong on it.
    ///
    /// Rendezvous hashing exists to bound movement: with `n` devices growing to
    /// `n + 1`, roughly `1 / (n + 1)` of keys should relocate. Anything close to
    /// total reshuffling means the mapping is not stable across topology
    /// changes, which turns every expansion into a full data migration.
    #[test]
    fn adding_a_device_moves_only_a_small_fraction_of_objects() {
        fn device_of(node: NodeId, class: &StorageClass, capacity: u64) -> DeviceRecord {
            DeviceRecord::legacy_directory(
                node,
                Some(std::path::PathBuf::from("/srv/record-store")),
                class.clone(),
                DeviceCapacity {
                    raw_bytes: capacity,
                    usable_bytes: capacity,
                    allocated_bytes: 0,
                    reserved_bytes: 0,
                    available_bytes: capacity,
                },
            )
        }

        fn cluster(count: usize) -> ClusterTopology {
            let nodes = (0..count)
                .map(|index| {
                    let mut node = record(
                        NodeState::Healthy,
                        &format!("rack-{index}"),
                        1 << 40,
                        1 << 40,
                    );
                    node.devices = vec![device_of(node.node_id, &node.storage_class, 1 << 40)];
                    node
                })
                .collect();
            topology(nodes)
        }

        // A deterministic key set, so the measurement is reproducible.
        let objects: Vec<ObjectId> = (0..2_000_u128)
            .map(|index| ObjectId::from_uuid(uuid::Uuid::from_u128(index)))
            .collect();

        let before = cluster(4);
        let mut expanded = before.nodes.clone();
        let mut extra = record(NodeState::Healthy, "rack-4", 1 << 40, 1 << 40);
        extra.devices = vec![device_of(extra.node_id, &extra.storage_class, 1 << 40)];
        expanded.push(extra);
        let after = ClusterTopology::at_epoch(
            before.cluster_id,
            before.config.clone(),
            expanded,
            before.epoch.next(),
        );

        let policy = CapacityAwarePlacement::new(None);
        let mut moved = 0_usize;
        for object in &objects {
            let request = ObjectPlacementRequest::new(*object, 1, 1, StorageClass::default())
                .with_size_hint(Some(10));
            let first = policy.place(&request, &before).expect("place before");
            let second = policy.place(&request, &after).expect("place after");
            if first.targets[0].device_id != second.targets[0].device_id {
                moved += 1;
            }
        }

        // Ideal is 1/5 of keys. Allow generous slack for hash variance, but
        // nothing like the total reshuffle an unstable mapping produces.
        let moved_permille = moved * 1_000 / objects.len();
        eprintln!("relocated {moved_permille}/1000 (rendezvous ideal is 200/1000)");
        assert!(
            moved_permille <= 300,
            "adding one device to a four-device cluster relocated {moved_permille}/1000 of objects; \
             placement is not stable across topology changes"
        );
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

    /// A local replica is written first so the first copy costs no network hop.
    ///
    /// The plan asks for as many replicas as there are nodes, so the local node
    /// is certainly among them: locality decides write *order*, never which
    /// devices are chosen, and asserting otherwise made this test depend on
    /// randomly generated node identifiers.
    #[test]
    fn a_selected_local_node_is_ordered_first() {
        let nodes = vec![
            record(NodeState::Healthy, "a", 1_000, 1_000),
            record(NodeState::Healthy, "b", 1_000, 1_000),
            record(NodeState::Healthy, "c", 1_000, 1_000),
        ];
        let local = nodes[2].node_id;
        let topology = topology(nodes);
        let plan = CapacityAwarePlacement::new(Some(local))
            .place(&request(3, 2), &topology)
            .expect("plan");
        assert_eq!(plan.targets[0].node_id, local);
        assert!(plan.targets[0].local);
        assert_eq!(plan.targets.len(), 3);
    }

    /// Every node must choose the same devices for the same object.
    ///
    /// Placement is recomputed independently on whichever node receives a
    /// request, so if the ingress node changed the outcome, two nodes would
    /// disagree about where an object belongs and repair would fight itself.
    #[test]
    fn the_ingress_node_changes_write_order_but_never_the_target_set() {
        let nodes = vec![
            record(NodeState::Healthy, "a", 1_000, 1_000),
            record(NodeState::Healthy, "b", 1_000, 1_000),
            record(NodeState::Healthy, "c", 1_000, 1_000),
        ];
        let identities: Vec<NodeId> = nodes.iter().map(|node| node.node_id).collect();
        let topology = topology(nodes);
        let request = request(2, 2);

        let baseline: BTreeSet<(NodeId, DeviceId)> = CapacityAwarePlacement::new(None)
            .place(&request, &topology)
            .expect("plan")
            .targets
            .iter()
            .map(|target| (target.node_id, target.device_id))
            .collect();

        for local in identities {
            let plan = CapacityAwarePlacement::new(Some(local))
                .place(&request, &topology)
                .expect("plan");
            let chosen: BTreeSet<(NodeId, DeviceId)> = plan
                .targets
                .iter()
                .map(|target| (target.node_id, target.device_id))
                .collect();
            assert_eq!(
                chosen, baseline,
                "the node computing the plan changed which devices were selected"
            );
            if plan.targets.iter().any(|target| target.node_id == local) {
                assert_eq!(
                    plan.targets[0].node_id, local,
                    "a selected local target must be written first"
                );
            }
        }
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
                            | PlacementError::NoEligibleDevices { .. }
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
