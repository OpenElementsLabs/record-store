//! Storage-device identity, inventory, capacity, health, and lifecycle.
//!
//! Device discovery is deliberately separate from registration. Discovering a
//! path or block device never gives Record Store ownership of it; only an
//! explicit, replicated registration makes it eligible for placement.

use std::{collections::BTreeMap, path::PathBuf};

use async_trait::async_trait;
use record_store_core::{DeviceId, NodeId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::topology::StorageClass;

/// Failures raised while validating or changing a device inventory.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DeviceError {
    /// A configured placement weight is outside the supported range.
    #[error("device placement weight must be between 1 and 10000")]
    InvalidWeight,
    /// Capacity fields contradict one another.
    #[error("invalid device capacity: {0}")]
    InvalidCapacity(String),
    /// Two inventory records use the same stable device identifier.
    #[error("device {0} is registered more than once")]
    DuplicateDevice(DeviceId),
    /// A record in a node inventory belongs to another node.
    #[error("device {device} belongs to node {actual}, not inventory owner {expected}")]
    WrongNode {
        /// Device with the inconsistent owner.
        device: DeviceId,
        /// Expected inventory owner.
        expected: NodeId,
        /// Owner recorded on the device.
        actual: NodeId,
    },
    /// A requested device is not registered.
    #[error("device {0} is not registered")]
    NotFound(DeviceId),
    /// A lifecycle transition is unsafe or meaningless.
    #[error("device state transition from {from} to {to} is not allowed")]
    InvalidStateTransition {
        /// Current state.
        from: DeviceState,
        /// Requested state.
        to: DeviceState,
    },
}

/// Physical storage technology.
///
/// This is hardware description, not business policy. Placement policy uses
/// [`StorageClass`] independently.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    /// An NVMe namespace.
    Nvme,
    /// A SATA-attached solid-state drive.
    SataSsd,
    /// A SAS-attached solid-state drive.
    SasSsd,
    /// A SATA-attached rotational drive.
    SataHdd,
    /// A SAS-attached rotational drive.
    SasHdd,
    /// A solid-state device whose bus is unknown.
    Ssd,
    /// A rotational device whose bus is unknown.
    Hdd,
    /// A generic block device.
    BlockDevice,
    /// A hardware or software RAID logical volume.
    RaidLogicalVolume,
    /// A cloud-provider block volume.
    CloudBlockVolume,
    /// A directory on a mounted filesystem.
    FilesystemDirectory,
    /// The platform did not expose a reliable kind.
    #[default]
    Unknown,
}

/// Stable administrator-configured contribution to placement weight.
///
/// `1000` is neutral. The bounded integer representation keeps replicated
/// state deterministic and avoids serializing floating-point policy values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct PlacementWeight(u32);

impl PlacementWeight {
    /// Neutral weight.
    pub const DEFAULT: u32 = 1_000;
    /// Largest accepted weight.
    pub const MAXIMUM: u32 = 10_000;

    /// Validates and constructs a weight.
    pub const fn new(value: u32) -> Result<Self, DeviceError> {
        if value == 0 || value > Self::MAXIMUM {
            Err(DeviceError::InvalidWeight)
        } else {
            Ok(Self(value))
        }
    }

    /// Returns the integer weight, where `1000` is neutral.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl Default for PlacementWeight {
    fn default() -> Self {
        Self(Self::DEFAULT)
    }
}

impl TryFrom<u32> for PlacementWeight {
    type Error = DeviceError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<PlacementWeight> for u32 {
    fn from(value: PlacementWeight) -> Self {
        value.0
    }
}

/// Capacity accounting for one independently placeable device.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCapacity {
    /// Physical or filesystem capacity exposed by the platform.
    pub raw_bytes: u64,
    /// Capacity available to Record Store after fixed reservations.
    pub usable_bytes: u64,
    /// Physical bytes allocated to committed replicas.
    pub allocated_bytes: u64,
    /// Capacity reserved for safety margins and in-flight work.
    pub reserved_bytes: u64,
    /// Capacity currently available for allocation.
    pub available_bytes: u64,
}

impl DeviceCapacity {
    /// Validates the accounting relationships.
    pub fn validate(self) -> Result<(), DeviceError> {
        if self.usable_bytes > self.raw_bytes {
            return Err(DeviceError::InvalidCapacity(
                "usable capacity exceeds raw capacity".into(),
            ));
        }
        if self.available_bytes > self.usable_bytes {
            return Err(DeviceError::InvalidCapacity(
                "available capacity exceeds usable capacity".into(),
            ));
        }
        if self.reserved_bytes > self.raw_bytes {
            return Err(DeviceError::InvalidCapacity(
                "reserved capacity exceeds raw capacity".into(),
            ));
        }
        Ok(())
    }

    /// Returns capacity consumed from the usable allocation pool.
    #[must_use]
    pub const fn used_bytes(self) -> u64 {
        self.usable_bytes.saturating_sub(self.available_bytes)
    }

    /// Returns utilization in parts per thousand.
    #[must_use]
    pub fn utilization_permille(self) -> u32 {
        if self.usable_bytes == 0 {
            return 1_000;
        }
        let scaled = u128::from(self.used_bytes()) * 1_000 / u128::from(self.usable_bytes);
        u32::try_from(scaled).unwrap_or(1_000).min(1_000)
    }
}

/// Best available device-health observation.
///
/// `Unknown` and `Unsupported` are first-class values. They are never replaced
/// by invented SMART, temperature, or wear information.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum DeviceHealth {
    /// The platform supplied no reliable observation.
    #[default]
    Unknown,
    /// Available observations report no problem.
    Healthy,
    /// The device is usable but has a material warning.
    Degraded,
    /// Integrity or I/O failures make the device unusable.
    Failed,
    /// The path or device cannot currently be reached.
    Unavailable,
    /// Health telemetry is not supported for this device kind or platform.
    Unsupported,
}

impl DeviceHealth {
    /// Returns whether health permits normal placement.
    #[must_use]
    pub const fn permits_placement(self) -> bool {
        matches!(self, Self::Unknown | Self::Healthy | Self::Unsupported)
    }

    /// Returns whether existing bytes may count toward durability.
    #[must_use]
    pub const fn contributes_durability(self) -> bool {
        !matches!(self, Self::Failed | Self::Unavailable)
    }
}

/// Durable administrative lifecycle of a registered device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceState {
    /// Observed by discovery but not registered or owned.
    Discovered,
    /// Registered and ready for an administrator to activate.
    Available,
    /// Eligible for new placement and normal I/O.
    Active,
    /// Retains data but receives no new placement while a warning is assessed.
    Degraded,
    /// Receives no new placement and is being evacuated.
    Draining,
    /// Administratively paused while retaining its data.
    Maintenance,
    /// Known unusable; its data no longer counts toward durability.
    Failed,
    /// Evacuation and policy verification completed.
    SafeToRemove,
    /// Permanently removed from placement and management workflows.
    Retired,
}

impl DeviceState {
    /// Returns whether the device may receive new data.
    #[must_use]
    pub const fn accepts_new_placements(self) -> bool {
        matches!(self, Self::Active)
    }

    /// Returns whether existing data may count toward durability.
    #[must_use]
    pub const fn contributes_durability(self) -> bool {
        matches!(
            self,
            Self::Active | Self::Degraded | Self::Draining | Self::Maintenance
        )
    }

    /// Returns whether this is a terminal lifecycle state.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Retired)
    }

    /// Returns whether a transition is permitted.
    // The arms are the transition table, and reading them as one is the point:
    // an unsafe device transition is a durability bug, so the permitted moves
    // stay legible rather than being folded into a single `matches!` pattern.
    #[allow(clippy::match_like_matches_macro)]
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        if self as u8 == next as u8 {
            return true;
        }
        match (self, next) {
            (Self::Discovered, Self::Available | Self::Retired) => true,
            (Self::Available, Self::Active | Self::Maintenance | Self::Retired) => true,
            (Self::Active, Self::Degraded | Self::Draining | Self::Maintenance | Self::Failed) => {
                true
            }
            (Self::Degraded, Self::Active | Self::Draining | Self::Maintenance | Self::Failed) => {
                true
            }
            (
                Self::Draining,
                Self::Active | Self::Maintenance | Self::Failed | Self::SafeToRemove,
            ) => true,
            (
                Self::Maintenance,
                Self::Available | Self::Active | Self::Draining | Self::Failed | Self::Retired,
            ) => true,
            (Self::Failed, Self::Draining | Self::SafeToRemove | Self::Retired) => true,
            (Self::SafeToRemove, Self::Active | Self::Retired) => true,
            _ => false,
        }
    }
}

impl std::fmt::Display for DeviceState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Discovered => "discovered",
            Self::Available => "available",
            Self::Active => "active",
            Self::Degraded => "degraded",
            Self::Draining => "draining",
            Self::Maintenance => "maintenance",
            Self::Failed => "failed",
            Self::SafeToRemove => "safe_to_remove",
            Self::Retired => "retired",
        })
    }
}

/// Optional facts exposed by a platform or administrator.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardwareMetadata {
    /// Manufacturer model string.
    pub model: Option<String>,
    /// Manufacturer serial number, when permission-safe to expose.
    pub serial: Option<String>,
    /// Controller identity.
    pub controller: Option<String>,
    /// Filesystem type.
    pub filesystem: Option<String>,
    /// Mounted filesystem root.
    pub mount_point: Option<PathBuf>,
    /// Whether the platform reports rotational media.
    pub rotational: Option<bool>,
    /// Logical sector or block size.
    pub logical_block_size: Option<u32>,
    /// Physical sector or block size.
    pub physical_block_size: Option<u32>,
    /// Temperature when a trustworthy source is available.
    pub temperature_celsius: Option<i16>,
    /// Remaining-life or wear percentage when a trustworthy source is available.
    pub wear_percentage: Option<u8>,
    /// SMART/NVMe health summary verbatim from a supported provider.
    pub health_summary: Option<String>,
}

/// One explicitly registered, independently managed placement resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRecord {
    /// Stable Record Store device identity.
    pub id: DeviceId,
    /// Node that owns and serves this device.
    pub node_id: NodeId,
    /// Current OS path. It is descriptive, never persistent identity.
    pub current_path: Option<PathBuf>,
    /// Stable platform identity such as a WWN or filesystem UUID.
    pub stable_hardware_identifier: Option<String>,
    /// Physical technology.
    pub kind: DeviceKind,
    /// Logical storage policy class.
    pub storage_class: StorageClass,
    /// Device-level capacity accounting.
    pub capacity: DeviceCapacity,
    /// Administrator-configured stable weight.
    pub configured_weight: PlacementWeight,
    /// Best available observed health.
    pub health: DeviceHealth,
    /// Durable lifecycle state.
    pub state: DeviceState,
    /// Optional platform facts.
    pub hardware: HardwareMetadata,
}

impl DeviceRecord {
    /// Deterministic compatibility identity for the historical per-node store.
    #[must_use]
    pub const fn legacy_id(node_id: NodeId) -> DeviceId {
        DeviceId::from_uuid(node_id.as_uuid())
    }

    /// Builds the compatibility device representing a node's original data root.
    #[must_use]
    pub fn legacy_directory(
        node_id: NodeId,
        path: Option<PathBuf>,
        storage_class: StorageClass,
        capacity: DeviceCapacity,
    ) -> Self {
        Self {
            id: Self::legacy_id(node_id),
            node_id,
            current_path: path,
            stable_hardware_identifier: None,
            kind: DeviceKind::FilesystemDirectory,
            storage_class,
            capacity,
            configured_weight: PlacementWeight::default(),
            health: DeviceHealth::Unknown,
            state: DeviceState::Active,
            hardware: HardwareMetadata::default(),
        }
    }

    /// Returns whether normal placement may select this device.
    #[must_use]
    pub fn eligible_for_placement(&self, reservation_bytes: u64, margin_bytes: u64) -> bool {
        self.state.accepts_new_placements()
            && self.health.permits_placement()
            && self.capacity.usable_bytes > 0
            && self.capacity.available_bytes >= reservation_bytes.saturating_add(margin_bytes)
    }

    /// Stable capacity-based weight used by rendezvous placement.
    #[must_use]
    pub fn effective_weight(&self) -> u128 {
        u128::from(self.capacity.usable_bytes)
            .saturating_mul(u128::from(self.configured_weight.get()))
            / u128::from(PlacementWeight::DEFAULT)
    }

    /// Applies a validated lifecycle transition.
    pub fn transition(&mut self, next: DeviceState) -> Result<bool, DeviceError> {
        if self.state == next {
            return Ok(false);
        }
        if !self.state.can_transition_to(next) {
            return Err(DeviceError::InvalidStateTransition {
                from: self.state,
                to: next,
            });
        }
        self.state = next;
        Ok(true)
    }
}

/// A discovered resource that Record Store does not yet own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredDevice {
    /// Current platform path.
    pub current_path: PathBuf,
    /// Stable platform identifier, if exposed.
    pub stable_hardware_identifier: Option<String>,
    /// Best-effort physical kind.
    pub kind: DeviceKind,
    /// Measured capacity.
    pub capacity: DeviceCapacity,
    /// Best available health observation.
    pub health: DeviceHealth,
    /// Optional platform facts.
    pub hardware: HardwareMetadata,
}

/// OS-specific, read-only device discovery boundary.
#[async_trait]
pub trait DeviceDiscovery: Send + Sync {
    /// Discovers resources without registering, formatting, mounting, or claiming them.
    async fn discover(&self) -> Result<Vec<DiscoveredDevice>, DeviceDiscoveryError>;
}

/// A failure to inspect platform device information.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("device discovery unavailable: {0}")]
pub struct DeviceDiscoveryError(pub String);

/// Validated device inventory for one node.
#[derive(Debug, Clone, Default)]
pub struct DeviceManager {
    devices: BTreeMap<DeviceId, DeviceRecord>,
}

impl DeviceManager {
    /// Validates and creates an inventory.
    pub fn new(
        node_id: NodeId,
        devices: impl IntoIterator<Item = DeviceRecord>,
    ) -> Result<Self, DeviceError> {
        let mut inventory = BTreeMap::new();
        for device in devices {
            if device.node_id != node_id {
                return Err(DeviceError::WrongNode {
                    device: device.id,
                    expected: node_id,
                    actual: device.node_id,
                });
            }
            device.capacity.validate()?;
            let id = device.id;
            if inventory.insert(id, device).is_some() {
                return Err(DeviceError::DuplicateDevice(id));
            }
        }
        Ok(Self { devices: inventory })
    }

    /// Returns one registered device.
    #[must_use]
    pub fn device(&self, id: DeviceId) -> Option<&DeviceRecord> {
        self.devices.get(&id)
    }

    /// Returns the inventory in stable identifier order.
    pub fn devices(&self) -> impl Iterator<Item = &DeviceRecord> {
        self.devices.values()
    }

    /// Applies a validated lifecycle transition.
    pub fn transition(&mut self, id: DeviceId, next: DeviceState) -> Result<bool, DeviceError> {
        self.devices
            .get_mut(&id)
            .ok_or(DeviceError::NotFound(id))?
            .transition(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(node_id: NodeId) -> DeviceRecord {
        DeviceRecord::legacy_directory(
            node_id,
            Some(PathBuf::from("/srv/record-store")),
            StorageClass::default(),
            DeviceCapacity {
                raw_bytes: 1_000,
                usable_bytes: 900,
                allocated_bytes: 100,
                reserved_bytes: 100,
                available_bytes: 800,
            },
        )
    }

    #[test]
    fn drain_must_complete_before_safe_removal() {
        let mut device = device(NodeId::new());
        assert!(device.transition(DeviceState::SafeToRemove).is_err());
        assert!(device.transition(DeviceState::Draining).expect("drain"));
        assert!(device.transition(DeviceState::SafeToRemove).expect("safe"));
        assert!(!device.state.accepts_new_placements());
    }

    #[test]
    fn inventory_rejects_cross_node_devices() {
        let owner = NodeId::new();
        let foreign = device(NodeId::new());
        assert!(matches!(
            DeviceManager::new(owner, [foreign]),
            Err(DeviceError::WrongNode { .. })
        ));
    }

    #[test]
    fn unknown_health_is_preserved_and_does_not_fabricate_telemetry() {
        let device = device(NodeId::new());
        assert_eq!(device.health, DeviceHealth::Unknown);
        assert_eq!(device.hardware.temperature_celsius, None);
        assert!(device.eligible_for_placement(1, 1));
    }
}
