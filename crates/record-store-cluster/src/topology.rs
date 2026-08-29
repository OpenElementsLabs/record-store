//! Cluster topology: node descriptors, failure domains, storage classes, and
//! the explicit node lifecycle state machine.

use std::{
    collections::BTreeMap,
    fmt::{self, Display, Formatter},
    str::FromStr,
};

use chrono::{DateTime, Utc};
use record_store_core::{ClusterId, NodeId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    config::{CapacityLevel, ClusterConfig},
    device::{DeviceCapacity, DeviceRecord},
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
/// Storage classes are operator-defined labels. Record Store only uses them to keep
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
    /// Spread across independently managed devices.
    Device,
    /// Only guarantee distinct nodes.
    Node,
    /// Spread across physical hosts.
    Host,
    /// Spread across racks.
    #[default]
    Rack,
    /// Spread across datacenters.
    Datacenter,
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
            Self::Device => None,
            Self::Node => None,
            Self::Host => Some("host"),
            Self::Rack => Some("rack"),
            Self::Datacenter => Some("datacenter"),
            Self::Zone => Some("zone"),
            Self::Region => Some("region"),
        }
    }
}

impl Display for FailureDomainScope {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Device => "device",
            Self::Node => "node",
            Self::Host => "host",
            Self::Rack => "rack",
            Self::Datacenter => "datacenter",
            Self::Zone => "zone",
            Self::Region => "region",
        })
    }
}

impl FromStr for FailureDomainScope {
    type Err = TopologyError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "device" => Ok(Self::Device),
            "node" => Ok(Self::Node),
            "host" => Ok(Self::Host),
            "rack" => Ok(Self::Rack),
            "datacenter" => Ok(Self::Datacenter),
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
/// physical layout without a Record Store release. Placement only needs a stable string
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
    /// Nodes missing the requested label all share one `unknown:<scope>` domain.
    /// That is deliberate: an unlabelled node is not evidence of separation, so
    /// treating each one as its own rack would let placement report rack
    /// separation it never verified. Sharing a domain instead makes strict
    /// policies refuse, and non-strict ones record `failure_domains_reused`.
    /// Operators who only want distinct nodes should ask for
    /// [`FailureDomainScope::Node`], which keys by node identifier.
    #[must_use]
    pub fn domain_key(&self, scope: FailureDomainScope, node_id: NodeId) -> String {
        match scope {
            FailureDomainScope::Device => "unknown:device".to_owned(),
            FailureDomainScope::Node => format!("node:{node_id}"),
            other => match other.label().and_then(|label| self.get(label)) {
                Some(value) => format!("{other}:{value}"),
                // Missing topology is unknown, not proof that two nodes occupy
                // separate racks, datacenters, regions, or physical hosts.
                None => format!("unknown:{other}"),
            },
        }
    }
}

/// Monotonic generation of the durable topology and placement policy.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct ClusterMapEpoch(u64);

impl ClusterMapEpoch {
    /// Initial committed generation.
    pub const INITIAL: Self = Self(1);

    /// Creates an epoch from durable state.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the stored generation.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Advances the generation without wrapping.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

impl Display for ClusterMapEpoch {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.0, formatter)
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
    /// Bytes occupied by Record Store replicas on this node.
    pub replica_bytes: u64,
    /// Bytes held by incomplete transfers and temporary uploads.
    pub temporary_bytes: u64,
}

impl NodeCapacity {
    /// Aggregates independently managed devices for compatibility status views.
    #[must_use]
    pub fn from_devices(devices: &[DeviceRecord]) -> Self {
        devices.iter().fold(Self::default(), |mut total, device| {
            total.total_bytes = total.total_bytes.saturating_add(device.capacity.raw_bytes);
            total.available_bytes = total
                .available_bytes
                .saturating_add(device.capacity.available_bytes);
            total.replica_bytes = total
                .replica_bytes
                .saturating_add(device.capacity.allocated_bytes);
            total
        })
    }

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
    /// Explicitly registered storage devices served by the node.
    #[serde(default)]
    pub devices: Vec<DeviceRecord>,
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
    /// Independently managed storage devices.
    #[serde(default)]
    pub devices: Vec<DeviceRecord>,
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
            devices: registration.devices,
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

    /// Returns one registered device.
    #[must_use]
    pub fn device(&self, device_id: record_store_core::DeviceId) -> Option<&DeviceRecord> {
        self.devices.iter().find(|device| device.id == device_id)
    }

    /// Returns mutable access to one registered device.
    #[must_use]
    pub fn device_mut(
        &mut self,
        device_id: record_store_core::DeviceId,
    ) -> Option<&mut DeviceRecord> {
        self.devices
            .iter_mut()
            .find(|device| device.id == device_id)
    }

    /// Installs the deterministic compatibility device for a pre-device record.
    pub fn ensure_legacy_device(&mut self) {
        if !self.devices.is_empty() {
            return;
        }
        let capacity = DeviceCapacity {
            raw_bytes: self.capacity.total_bytes,
            usable_bytes: self.capacity.total_bytes,
            allocated_bytes: self.capacity.replica_bytes,
            reserved_bytes: 0,
            available_bytes: self.capacity.available_bytes,
        };
        self.devices.push(DeviceRecord::legacy_directory(
            self.node_id,
            None,
            self.storage_class.clone(),
            capacity,
        ));
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
    /// Durable topology and placement-policy generation.
    #[serde(default)]
    pub epoch: ClusterMapEpoch,
    /// Known members, ordered by node identifier.
    pub nodes: Vec<NodeRecord>,
}

impl ClusterTopology {
    /// Creates a topology view with deterministic node ordering.
    #[must_use]
    pub fn new(cluster_id: ClusterId, config: ClusterConfig, mut nodes: Vec<NodeRecord>) -> Self {
        for node in &mut nodes {
            node.ensure_legacy_device();
        }
        nodes.sort_by_key(|node| node.node_id);
        Self {
            cluster_id,
            config,
            epoch: ClusterMapEpoch::default(),
            nodes,
        }
    }

    /// Creates a topology at a committed placement epoch.
    #[must_use]
    pub fn at_epoch(
        cluster_id: ClusterId,
        config: ClusterConfig,
        nodes: Vec<NodeRecord>,
        epoch: ClusterMapEpoch,
    ) -> Self {
        let mut topology = Self::new(cluster_id, config, nodes);
        topology.epoch = epoch;
        topology
    }

    /// Returns one node record.
    #[must_use]
    pub fn node(&self, node_id: NodeId) -> Option<&NodeRecord> {
        self.nodes.iter().find(|node| node.node_id == node_id)
    }

    /// Returns one device and its owning node.
    #[must_use]
    pub fn device(
        &self,
        device_id: record_store_core::DeviceId,
    ) -> Option<(&NodeRecord, &DeviceRecord)> {
        self.nodes
            .iter()
            .find_map(|node| node.device(device_id).map(|device| (node, device)))
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

    /// Returns every registered device in stable `(node, device)` order.
    pub fn devices(&self) -> impl Iterator<Item = (&NodeRecord, &DeviceRecord)> {
        self.nodes
            .iter()
            .flat_map(|node| node.devices.iter().map(move |device| (node, device)))
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

    /// Returns whether an exact node/device target counts toward durability.
    #[must_use]
    pub fn device_contributes_durability(
        &self,
        node_id: NodeId,
        device_id: record_store_core::DeviceId,
    ) -> bool {
        self.node(node_id).is_some_and(|node| {
            node.state.contributes_durability()
                && node.device(device_id).is_some_and(|device| {
                    device.state.contributes_durability() && device.health.contributes_durability()
                })
        })
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::catalog::test_support::registration;

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
        // An unlabelled node shares the unknown domain rather than being counted
        // as a zone of its own, so placement cannot claim unverified separation.
        let other = NodeId::new();
        assert_eq!(
            FailureDomain::default().domain_key(FailureDomainScope::Zone, node),
            "unknown:zone"
        );
        assert_eq!(
            FailureDomain::default().domain_key(FailureDomainScope::Zone, other),
            FailureDomain::default().domain_key(FailureDomainScope::Zone, node)
        );
        // Node scope is the way to ask for per-node separation.
        assert_eq!(
            FailureDomain::default().domain_key(FailureDomainScope::Node, node),
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

    /// The lifecycle is a state machine an operator drives, and a decommissioned
    /// node must never come back. Allowing that would resurrect a node whose
    /// replicas the cluster already redistributed.
    #[test]
    fn a_terminal_state_can_never_be_left() {
        assert!(NodeState::Decommissioned.is_terminal());
        for next in [
            NodeState::Joining,
            NodeState::Healthy,
            NodeState::Suspect,
            NodeState::Draining,
            NodeState::Maintenance,
            NodeState::Offline,
            NodeState::Unreachable,
        ] {
            assert!(
                !NodeState::Decommissioned.can_transition_to(next),
                "a decommissioned node must not become {next:?}"
            );
        }
        assert!(
            NodeState::Decommissioned.can_transition_to(NodeState::Decommissioned),
            "restating the same state is always allowed"
        );
    }

    /// Only some states let the cluster place new replicas, serve reads, or
    /// count towards durability. Confusing them either overloads a draining node
    /// or hides a genuine durability shortfall.
    #[test]
    fn each_state_answers_the_three_scheduling_questions_distinctly() {
        for (state, placement, reads, durability) in [
            (NodeState::Joining, false, false, false),
            (NodeState::Healthy, true, true, true),
            (NodeState::Suspect, false, true, true),
            (NodeState::Unreachable, false, false, true),
            (NodeState::Draining, false, true, true),
            (NodeState::Maintenance, false, false, true),
            (NodeState::Offline, false, false, false),
            (NodeState::Decommissioned, false, false, false),
        ] {
            assert_eq!(
                state.accepts_new_replicas(),
                placement,
                "{state:?} placement"
            );
            assert_eq!(state.serves_reads(), reads, "{state:?} reads");
            assert_eq!(
                state.contributes_durability(),
                durability,
                "{state:?} durability"
            );
        }
    }

    /// A transition to the state a node already holds is a no-op that still
    /// records a fresh reason, so a repeated administrative action is safe.
    #[test]
    fn restating_the_current_state_is_a_no_op_that_still_records_a_reason() {
        let now = Utc::now();
        let mut record = NodeRecord::joining(registration(), 1, true, now);
        record
            .transition(NodeState::Healthy, Some("joined".into()), now)
            .expect("transition");

        let changed = record
            .transition(NodeState::Healthy, Some("still fine".into()), now)
            .expect("no-op transition");
        assert!(!changed);
        assert_eq!(record.state_reason.as_deref(), Some("still fine"));
    }

    #[test]
    fn an_impossible_transition_is_refused_and_leaves_the_state_alone() {
        let now = Utc::now();
        let mut record = NodeRecord::joining(registration(), 1, true, now);
        record
            .transition(NodeState::Decommissioned, None, now)
            .expect("decommission");

        let result = record.transition(NodeState::Healthy, None, now);
        assert!(matches!(
            result,
            Err(TopologyError::InvalidStateTransition { .. })
        ));
        assert_eq!(record.state, NodeState::Decommissioned);
    }

    /// Utilization drives placement ordering, so the arithmetic has to hold at
    /// the edges: an empty node is 0% and a full one is 100%, never more.
    #[test]
    fn utilization_is_reported_on_both_scales_without_overflowing() {
        let empty = NodeCapacity {
            total_bytes: 1_000,
            available_bytes: 1_000,
            replica_bytes: 0,
            temporary_bytes: 0,
        };
        assert_eq!(empty.utilization_percent(), 0);
        assert_eq!(empty.utilization_permille(), 0);

        let full = NodeCapacity {
            total_bytes: 1_000,
            available_bytes: 0,
            replica_bytes: 1_000,
            temporary_bytes: 0,
        };
        assert_eq!(full.utilization_percent(), 100);
        assert_eq!(full.utilization_permille(), 1_000);

        let half = NodeCapacity {
            total_bytes: 1_000,
            available_bytes: 500,
            replica_bytes: 500,
            temporary_bytes: 0,
        };
        assert_eq!(half.utilization_percent(), 50);
        assert_eq!(half.utilization_permille(), 500);
    }

    /// A node reporting no capacity at all must not divide by zero, and must not
    /// look like the emptiest node in the cluster.
    #[test]
    fn a_node_reporting_no_capacity_does_not_look_empty() {
        let unknown = NodeCapacity {
            total_bytes: 0,
            available_bytes: 0,
            replica_bytes: 0,
            temporary_bytes: 0,
        };
        assert_eq!(unknown.utilization_percent(), 100);
    }

    /// Placement eligibility combines the lifecycle state with how full the node
    /// is; either one alone is not enough to keep writing to it.
    #[test]
    fn placement_eligibility_needs_both_a_usable_state_and_room() {
        let config = ClusterConfig::default();
        let now = Utc::now();
        let mut record = NodeRecord::joining(registration(), 1, true, now);
        record
            .transition(NodeState::Healthy, None, now)
            .expect("healthy");
        record.capacity = NodeCapacity {
            total_bytes: 1_000,
            available_bytes: 900,
            replica_bytes: 100,
            temporary_bytes: 0,
        };
        assert!(record.eligible_for_placement(&config));

        record.capacity.available_bytes = 0;
        record.capacity.replica_bytes = 1_000;
        assert!(
            !record.eligible_for_placement(&config),
            "a full node takes no new replicas"
        );

        record.capacity.available_bytes = 900;
        record.capacity.replica_bytes = 100;
        record
            .transition(NodeState::Draining, None, now)
            .expect("drain");
        assert!(
            !record.eligible_for_placement(&config),
            "a draining node takes no new replicas even with room"
        );
    }

    /// The topology view is what every scheduler reads, so its filters have to
    /// separate members, healthy nodes, and placement candidates correctly.
    #[test]
    fn the_topology_view_separates_members_from_placement_candidates() {
        let now = Utc::now();
        let config = ClusterConfig::default();
        let mut healthy = NodeRecord::joining(registration(), 1, true, now);
        healthy
            .transition(NodeState::Healthy, None, now)
            .expect("healthy");
        let mut draining = NodeRecord::joining(registration(), 2, true, now);
        draining
            .transition(NodeState::Healthy, None, now)
            .expect("healthy");
        draining
            .transition(NodeState::Draining, None, now)
            .expect("draining");
        let joining = NodeRecord::joining(registration(), 3, true, now);

        let topology = ClusterTopology::new(
            record_store_core::ClusterId::new(),
            config,
            vec![healthy.clone(), draining.clone(), joining.clone()],
        );

        assert_eq!(topology.members().count(), 3);
        assert_eq!(topology.healthy().count(), 1);
        assert_eq!(topology.placeable().count(), 1);
        assert!(topology.node(healthy.node_id).is_some());
        assert!(topology.node(record_store_core::NodeId::new()).is_none());
        assert!(topology.serves_reads(draining.node_id));
        assert!(!topology.serves_reads(joining.node_id));
    }
}
