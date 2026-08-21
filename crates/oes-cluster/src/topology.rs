//! Cluster topology: node descriptors, failure domains, storage classes, and
//! the explicit node lifecycle state machine.

use std::{
    collections::BTreeMap,
    fmt::{self, Display, Formatter},
    str::FromStr,
};

use chrono::{DateTime, Utc};
use oes_core::{ClusterId, NodeId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    config::{CapacityLevel, ClusterConfig},
    identity::RaftNodeId,
    version::{NodeVersions, ProtocolVersion},
};

/// Failures while constructing topology values.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TopologyError {
    /// A storage-class label was malformed.
    #[error("invalid storage class: {0}")]
    InvalidStorageClass(String),
    /// A failure-domain label key or value was malformed.
    #[error("invalid failure-domain label: {0}")]
    InvalidFailureDomain(String),
    /// A node lifecycle transition is not allowed.
    #[error("node state transition from {from} to {to} is not allowed")]
    InvalidStateTransition {
        /// Current state.
        from: NodeState,
        /// Requested state.
        to: NodeState,
    },
    /// An advertised RPC address was malformed.
    #[error("invalid node address: {0}")]
    InvalidAddress(String),
    /// A node state name was not recognized.
    #[error("unknown node state '{0}'")]
    UnknownNodeState(String),
}

/// A validated storage-class label such as `nvme`, `ssd`, `hdd`, or `archive`.
///
/// Storage classes are operator-defined labels. OES only uses them to keep
/// replicas of one payload on compatible hardware; it performs no automatic
/// tiering.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct StorageClass(String);

impl StorageClass {
    /// Maximum label length.
    pub const MAX_LENGTH: usize = 32;
    /// Class assigned when an operator does not choose one.
    pub const DEFAULT: &'static str = "standard";

    /// Validates and creates a storage class.
    pub fn new(value: impl Into<String>) -> Result<Self, TopologyError> {
        let value = value.into();
        if value.is_empty() || value.len() > Self::MAX_LENGTH {
            return Err(TopologyError::InvalidStorageClass(
                "class must contain between 1 and 32 bytes".into(),
            ));
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(TopologyError::InvalidStorageClass(
                "class may only contain lowercase letters, digits, and hyphens".into(),
            ));
        }
        Ok(Self(value))
    }

    /// Returns the validated label.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for StorageClass {
    fn default() -> Self {
        Self(Self::DEFAULT.to_owned())
    }
}

impl Display for StorageClass {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for StorageClass {
    type Err = TopologyError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl TryFrom<String> for StorageClass {
    type Error = TopologyError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<StorageClass> for String {
    fn from(value: StorageClass) -> Self {
        value.0
    }
}

/// The topology scope replicas are spread across.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureDomainScope {
    /// Only guarantee distinct nodes.
    Node,
    /// Spread across physical hosts.
    Host,
    /// Spread across racks.
    #[default]
    Rack,
    /// Spread across availability zones.
    Zone,
    /// Spread across regions.
    Region,
}

impl FailureDomainScope {
    /// Returns the failure-domain label this scope groups by.
    #[must_use]
    pub const fn label(self) -> Option<&'static str> {
        match self {
            Self::Node => None,
            Self::Host => Some("host"),
            Self::Rack => Some("rack"),
            Self::Zone => Some("zone"),
            Self::Region => Some("region"),
        }
    }
}

impl Display for FailureDomainScope {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Node => "node",
            Self::Host => "host",
            Self::Rack => "rack",
            Self::Zone => "zone",
            Self::Region => "region",
        })
    }
}

impl FromStr for FailureDomainScope {
    type Err = TopologyError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "node" => Ok(Self::Node),
            "host" => Ok(Self::Host),
            "rack" => Ok(Self::Rack),
            "zone" => Ok(Self::Zone),
            "region" => Ok(Self::Region),
            other => Err(TopologyError::InvalidFailureDomain(format!(
                "unknown failure-domain scope '{other}'"
            ))),
        }
    }
}

/// A flexible set of topology labels such as `region`, `zone`, `rack`, `host`.
///
/// The label system is deliberately open so operators can describe their own
/// physical layout without an OES release. Placement only needs a stable string
/// per scope.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FailureDomain {
    labels: BTreeMap<String, String>,
}

impl FailureDomain {
    /// Maximum number of labels on one node.
    pub const MAX_LABELS: usize = 16;
    /// Maximum length of a label key or value.
    pub const MAX_LABEL_LENGTH: usize = 63;

    /// Creates a validated label set.
    pub fn new(labels: BTreeMap<String, String>) -> Result<Self, TopologyError> {
        if labels.len() > Self::MAX_LABELS {
            return Err(TopologyError::InvalidFailureDomain(format!(
                "at most {} labels are allowed",
                Self::MAX_LABELS
            )));
        }
        for (key, value) in &labels {
            validate_label(key)?;
            validate_label(value)?;
        }
        Ok(Self { labels })
    }

    /// Parses `region=ug-central,zone=dc1,rack=rack-04` style specifications.
    pub fn parse(specification: &str) -> Result<Self, TopologyError> {
        let mut labels = BTreeMap::new();
        for entry in specification.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let (key, value) = entry.split_once('=').ok_or_else(|| {
                TopologyError::InvalidFailureDomain(format!("'{entry}' is not key=value"))
            })?;
            labels.insert(key.trim().to_owned(), value.trim().to_owned());
        }
        Self::new(labels)
    }

    /// Returns a label value.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.labels.get(key).map(String::as_str)
    }

    /// Returns all labels in deterministic order.
    #[must_use]
    pub const fn labels(&self) -> &BTreeMap<String, String> {
        &self.labels
    }

    /// Returns the grouping key for a scope.
    ///
    /// Nodes without the requested label are treated as their own domain, keyed
    /// by node identifier, so a partially labelled cluster never collapses every
    /// node into one shared domain.
    #[must_use]
    pub fn domain_key(&self, scope: FailureDomainScope, node_id: NodeId) -> String {
        match scope.label().and_then(|label| self.get(label)) {
            Some(value) => format!("{scope}:{value}"),
            None => format!("node:{node_id}"),
        }
    }
}

fn validate_label(value: &str) -> Result<(), TopologyError> {
    if value.is_empty() || value.len() > FailureDomain::MAX_LABEL_LENGTH {
        return Err(TopologyError::InvalidFailureDomain(
            "label keys and values must contain between 1 and 63 bytes".into(),
        ));
    }
    if !value.bytes().all(|byte| {
        byte.is_ascii_lowercase()
            || byte.is_ascii_uppercase()
            || byte.is_ascii_digit()
            || matches!(byte, b'-' | b'_' | b'.')
    }) {
        return Err(TopologyError::InvalidFailureDomain(
            "label keys and values may only contain letters, digits, '-', '_', and '.'".into(),
        ));
    }
    Ok(())
}

/// Explicit node lifecycle state.
///
/// Health is deliberately not a boolean: administrative intent (`Draining`,
/// `Maintenance`, `Decommissioned`) must never be confused with observed
/// reachability (`Suspect`, `Unreachable`, `Offline`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    /// Registered and reconciling; not yet eligible for new replicas.
    Joining,
    /// Fully participating.
    Healthy,
    /// Heartbeats are late. Not yet treated as data loss.
    Suspect,
    /// Internal RPC to the node is actively failing.
    Unreachable,
    /// Administratively moving its replicas elsewhere.
    Draining,
    /// Administratively paused; retains data and takes no new replicas.
    Maintenance,
    /// Considered unavailable long enough that its replicas are repaired away.
    Offline,
    /// Permanently removed from the cluster.
    Decommissioned,
}

impl NodeState {
    /// Returns whether the node may receive newly placed replicas.
    #[must_use]
    pub const fn accepts_new_replicas(self) -> bool {
        matches!(self, Self::Healthy)
    }

    /// Returns whether the node's replicas may be read.
    #[must_use]
    pub const fn serves_reads(self) -> bool {
        matches!(self, Self::Healthy | Self::Suspect | Self::Draining)
    }

    /// Returns whether the node's replicas still count towards durability.
    ///
    /// A `Suspect` or `Unreachable` node is not yet assumed lost: repairing
    /// immediately after a transient network failure causes recovery storms.
    #[must_use]
    pub const fn contributes_durability(self) -> bool {
        matches!(
            self,
            Self::Healthy | Self::Suspect | Self::Unreachable | Self::Draining | Self::Maintenance
        )
    }

    /// Returns whether the node is expected to be reachable right now.
    #[must_use]
    pub const fn expected_reachable(self) -> bool {
        matches!(
            self,
            Self::Joining | Self::Healthy | Self::Suspect | Self::Draining | Self::Maintenance
        )
    }

    /// Returns whether the node has been permanently removed.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Decommissioned)
    }

    /// Returns whether the state was chosen by an administrator.
    ///
    /// Administrative states are never changed by failure detection.
    #[must_use]
    pub const fn is_administrative(self) -> bool {
        matches!(
            self,
            Self::Draining | Self::Maintenance | Self::Decommissioned
        )
    }

    /// Returns whether this transition is permitted.
    ///
    /// Repeating the current state is always allowed so that administrative
    /// commands such as `drain` remain safely idempotent.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        if self as u8 == next as u8 {
            return true;
        }
        if self.is_terminal() {
            return false;
        }
        #[expect(
            clippy::match_like_matches_macro,
            reason = "the transition table stays readable as one arm per source state"
        )]
        match (self, next) {
            (Self::Joining, Self::Healthy | Self::Suspect | Self::Unreachable | Self::Offline) => {
                true
            }
            (Self::Joining, Self::Maintenance | Self::Decommissioned) => true,
            (
                Self::Healthy,
                Self::Suspect
                | Self::Unreachable
                | Self::Offline
                | Self::Draining
                | Self::Maintenance
                | Self::Decommissioned,
            ) => true,
            (
                Self::Suspect,
                Self::Healthy
                | Self::Unreachable
                | Self::Offline
                | Self::Draining
                | Self::Maintenance
                | Self::Decommissioned,
            ) => true,
            (
                Self::Unreachable,
                Self::Healthy
                | Self::Suspect
                | Self::Offline
                | Self::Draining
                | Self::Maintenance
                | Self::Decommissioned,
            ) => true,
            (Self::Offline, Self::Joining | Self::Healthy | Self::Suspect | Self::Unreachable) => {
                true
            }
            (Self::Offline, Self::Maintenance | Self::Decommissioned) => true,
            (Self::Draining, Self::Healthy | Self::Maintenance | Self::Decommissioned) => true,
            (Self::Draining, Self::Suspect | Self::Unreachable | Self::Offline) => true,
            (Self::Maintenance, Self::Healthy | Self::Draining | Self::Decommissioned) => true,
            (Self::Maintenance, Self::Suspect | Self::Unreachable | Self::Offline) => true,
            _ => false,
        }
    }
}

impl Display for NodeState {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Joining => "joining",
            Self::Healthy => "healthy",
            Self::Suspect => "suspect",
            Self::Unreachable => "unreachable",
            Self::Draining => "draining",
            Self::Maintenance => "maintenance",
            Self::Offline => "offline",
            Self::Decommissioned => "decommissioned",
        })
    }
}

impl FromStr for NodeState {
    type Err = TopologyError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "joining" => Ok(Self::Joining),
            "healthy" => Ok(Self::Healthy),
            "suspect" => Ok(Self::Suspect),
            "unreachable" => Ok(Self::Unreachable),
            "draining" => Ok(Self::Draining),
            "maintenance" => Ok(Self::Maintenance),
            "offline" => Ok(Self::Offline),
            "decommissioned" => Ok(Self::Decommissioned),
            other => Err(TopologyError::UnknownNodeState(other.to_owned())),
        }
    }
}

/// Filesystem capacity reported by a node.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeCapacity {
    /// Total bytes on the filesystem holding the data directory.
    pub total_bytes: u64,
    /// Currently free bytes.
    pub available_bytes: u64,
    /// Bytes occupied by OES replicas on this node.
    pub replica_bytes: u64,
    /// Bytes held by incomplete transfers and temporary uploads.
    pub temporary_bytes: u64,
}

impl NodeCapacity {
    /// Returns used bytes derived from the reported totals.
    #[must_use]
    pub const fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.available_bytes)
    }

    /// Returns utilization scaled to whole percent.
    ///
    /// A node reporting no capacity is treated as full, so an unmeasurable
    /// filesystem never attracts new replicas.
    #[must_use]
    pub fn utilization_percent(&self) -> u32 {
        self.utilization_scaled(100)
    }

    /// Returns utilization scaled to parts per thousand.
    ///
    /// Placement uses the finer scale so that large clusters still order nodes
    /// meaningfully when they sit inside the same whole percent.
    #[must_use]
    pub fn utilization_permille(&self) -> u32 {
        self.utilization_scaled(1_000)
    }

    fn utilization_scaled(&self, scale: u32) -> u32 {
        if self.total_bytes == 0 {
            return scale;
        }
        let scaled =
            u128::from(self.used_bytes()) * u128::from(scale) / u128::from(self.total_bytes);
        u32::try_from(scaled).unwrap_or(scale).min(scale)
    }
}

/// Live counters a node reports with its heartbeat.
///
/// Only bounded, low-cardinality aggregates belong here: heartbeats must never
/// carry per-object information.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeActivity {
    /// Uploads currently streaming into this node.
    pub active_uploads: u32,
    /// Downloads currently streaming out of this node.
    pub active_downloads: u32,
    /// Replica transfers this node still owes.
    pub replication_backlog: u64,
    /// Replicas that failed integrity verification since start-up.
    pub integrity_failures: u64,
}

/// What a node reports when it registers with the cluster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRegistration {
    /// Stable opaque node identifier.
    pub node_id: NodeId,
    /// Versions advertised by the node.
    pub versions: NodeVersions,
    /// Address peers should use to reach this node's internal RPC listener.
    pub rpc_address: String,
    /// Address S3 clients may use, when the node is reachable from clients.
    pub s3_endpoint: Option<String>,
    /// Operator-assigned storage class.
    pub storage_class: StorageClass,
    /// Operator-assigned topology labels.
    pub failure_domain: FailureDomain,
    /// Capacity measured locally by the node.
    pub capacity: NodeCapacity,
    /// Process start time, used to detect restarts.
    pub started_at: DateTime<Utc>,
}

/// The replicated record describing one cluster member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRecord {
    /// Stable opaque node identifier.
    pub node_id: NodeId,
    /// Consensus member identifier.
    pub raft_id: RaftNodeId,
    /// Internal protocol version last advertised.
    pub protocol: ProtocolVersion,
    /// Build version last advertised.
    pub software_version: String,
    /// Durable replica layout version last advertised.
    pub storage_format_version: u32,
    /// Internal RPC address.
    pub rpc_address: String,
    /// Optional client-facing S3 endpoint.
    pub s3_endpoint: Option<String>,
    /// Storage class.
    pub storage_class: StorageClass,
    /// Topology labels.
    pub failure_domain: FailureDomain,
    /// Current lifecycle state.
    pub state: NodeState,
    /// Whether the node votes in the metadata consensus group.
    pub metadata_voter: bool,
    /// Last capacity report.
    pub capacity: NodeCapacity,
    /// Last activity report.
    pub activity: NodeActivity,
    /// First time the node joined.
    pub joined_at: DateTime<Utc>,
    /// Last reported process start time.
    pub started_at: DateTime<Utc>,
    /// Last accepted heartbeat.
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    /// Time the current state was entered.
    pub state_changed_at: DateTime<Utc>,
    /// Operator-visible reason for the current state.
    pub state_reason: Option<String>,
}

impl NodeRecord {
    /// Creates a record for a newly registered node in the `Joining` state.
    #[must_use]
    pub fn joining(
        registration: NodeRegistration,
        raft_id: RaftNodeId,
        metadata_voter: bool,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            node_id: registration.node_id,
            raft_id,
            protocol: registration.versions.protocol,
            software_version: registration.versions.software,
            storage_format_version: registration.versions.storage_format,
            rpc_address: registration.rpc_address,
            s3_endpoint: registration.s3_endpoint,
            storage_class: registration.storage_class,
            failure_domain: registration.failure_domain,
            state: NodeState::Joining,
            metadata_voter,
            capacity: registration.capacity,
            activity: NodeActivity::default(),
            joined_at: now,
            started_at: registration.started_at,
            last_heartbeat_at: Some(now),
            state_changed_at: now,
            state_reason: Some("registered".into()),
        }
    }

    /// Returns the grouping key for a failure-domain scope.
    #[must_use]
    pub fn domain_key(&self, scope: FailureDomainScope) -> String {
        self.failure_domain.domain_key(scope, self.node_id)
    }

    /// Returns the capacity pressure level under the supplied thresholds.
    #[must_use]
    pub fn capacity_level(&self, config: &ClusterConfig) -> CapacityLevel {
        config.watermarks.level(self.capacity.utilization_percent())
    }

    /// Returns whether new replicas may be placed here.
    #[must_use]
    pub fn eligible_for_placement(&self, config: &ClusterConfig) -> bool {
        self.state.accepts_new_replicas() && self.capacity_level(config).accepts_new_replicas()
    }

    /// Applies a validated lifecycle transition.
    pub fn transition(
        &mut self,
        next: NodeState,
        reason: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<bool, TopologyError> {
        if self.state == next {
            if reason.is_some() {
                self.state_reason = reason;
            }
            return Ok(false);
        }
        if !self.state.can_transition_to(next) {
            return Err(TopologyError::InvalidStateTransition {
                from: self.state,
                to: next,
            });
        }
        self.state = next;
        self.state_changed_at = now;
        self.state_reason = reason;
        Ok(true)
    }
}

/// An immutable view of the cluster used by placement and scheduling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterTopology {
    /// Cluster identity.
    pub cluster_id: ClusterId,
    /// Cluster-wide configuration in effect.
    pub config: ClusterConfig,
    /// Known members, ordered by node identifier.
    pub nodes: Vec<NodeRecord>,
}

impl ClusterTopology {
    /// Creates a topology view with deterministic node ordering.
    #[must_use]
    pub fn new(cluster_id: ClusterId, config: ClusterConfig, mut nodes: Vec<NodeRecord>) -> Self {
        nodes.sort_by_key(|node| node.node_id);
        Self {
            cluster_id,
            config,
            nodes,
        }
    }

    /// Returns one node record.
    #[must_use]
    pub fn node(&self, node_id: NodeId) -> Option<&NodeRecord> {
        self.nodes.iter().find(|node| node.node_id == node_id)
    }

    /// Returns the node owning a consensus member identifier.
    #[must_use]
    pub fn node_by_raft_id(&self, raft_id: RaftNodeId) -> Option<&NodeRecord> {
        self.nodes.iter().find(|node| node.raft_id == raft_id)
    }

    /// Returns members excluding permanently removed nodes.
    pub fn members(&self) -> impl Iterator<Item = &NodeRecord> {
        self.nodes.iter().filter(|node| !node.state.is_terminal())
    }

    /// Returns nodes currently in the `Healthy` state.
    pub fn healthy(&self) -> impl Iterator<Item = &NodeRecord> {
        self.nodes
            .iter()
            .filter(|node| node.state == NodeState::Healthy)
    }

    /// Returns nodes eligible to receive new replicas.
    pub fn placeable(&self) -> impl Iterator<Item = &NodeRecord> {
        self.nodes
            .iter()
            .filter(|node| node.eligible_for_placement(&self.config))
    }

    /// Returns aggregated capacity across non-terminal members.
    #[must_use]
    pub fn capacity(&self) -> NodeCapacity {
        self.members()
            .fold(NodeCapacity::default(), |mut total, node| {
                total.total_bytes = total.total_bytes.saturating_add(node.capacity.total_bytes);
                total.available_bytes = total
                    .available_bytes
                    .saturating_add(node.capacity.available_bytes);
                total.replica_bytes = total
                    .replica_bytes
                    .saturating_add(node.capacity.replica_bytes);
                total.temporary_bytes = total
                    .temporary_bytes
                    .saturating_add(node.capacity.temporary_bytes);
                total
            })
    }

    /// Returns whether the node's replicas may currently be read.
    #[must_use]
    pub fn serves_reads(&self, node_id: NodeId) -> bool {
        self.node(node_id).is_some_and(|node| {
            node.state.serves_reads()
                || (node.state == NodeState::Maintenance && self.config.maintenance_serves_reads)
        })
    }

    /// Returns whether the node's replicas still count towards durability.
    #[must_use]
    pub fn contributes_durability(&self, node_id: NodeId) -> bool {
        self.node(node_id)
            .is_some_and(|node| node.state.contributes_durability())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_classes_reject_unsafe_labels() {
        for value in ["", "NVME", "ssd_fast", "a".repeat(33).as_str()] {
            assert!(StorageClass::new(value).is_err(), "accepted {value}");
        }
        assert_eq!(
            StorageClass::new("nvme-1").expect("valid class").as_str(),
            "nvme-1"
        );
    }

    #[test]
    fn failure_domains_parse_and_group() {
        let domain =
            FailureDomain::parse("region=ug-central, zone=dc1,rack=rack-04").expect("valid labels");
        assert_eq!(domain.get("zone"), Some("dc1"));
        let node = NodeId::new();
        assert_eq!(
            domain.domain_key(FailureDomainScope::Zone, node),
            "zone:dc1"
        );
        assert_eq!(
            FailureDomain::default().domain_key(FailureDomainScope::Zone, node),
            format!("node:{node}")
        );
    }

    #[test]
    fn decommissioned_nodes_never_transition_again() {
        for state in [
            NodeState::Joining,
            NodeState::Healthy,
            NodeState::Suspect,
            NodeState::Unreachable,
            NodeState::Draining,
            NodeState::Maintenance,
            NodeState::Offline,
            NodeState::Decommissioned,
        ] {
            assert!(
                !NodeState::Decommissioned.can_transition_to(state)
                    || state == NodeState::Decommissioned,
                "decommissioned must stay terminal for {state}"
            );
        }
    }

    #[test]
    fn only_healthy_nodes_accept_new_replicas() {
        assert!(NodeState::Healthy.accepts_new_replicas());
        for state in [
            NodeState::Joining,
            NodeState::Suspect,
            NodeState::Unreachable,
            NodeState::Draining,
            NodeState::Maintenance,
            NodeState::Offline,
            NodeState::Decommissioned,
        ] {
            assert!(!state.accepts_new_replicas(), "{state} accepted placement");
        }
    }

    #[test]
    fn offline_and_decommissioned_replicas_do_not_count_for_durability() {
        assert!(!NodeState::Offline.contributes_durability());
        assert!(!NodeState::Decommissioned.contributes_durability());
        assert!(NodeState::Suspect.contributes_durability());
    }
}
