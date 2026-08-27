//! Cluster-wide configuration.
//!
//! These values are replicated through consensus and apply to every node.
//! Node-local settings such as bind addresses, data directories, and TLS key
//! paths deliberately live in the process configuration file instead: mixing the
//! two makes it impossible to reason about which value a node is actually using.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::topology::FailureDomainScope;

/// Highest replication factor Record Store accepts in this release.
pub const MAXIMUM_REPLICATION_FACTOR: u8 = 3;

/// Failures raised while validating cluster configuration.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid cluster configuration: {0}")]
pub struct ClusterConfigError(pub String);

/// Capacity pressure thresholds expressed in whole percent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacityWatermarks {
    /// Below this utilization a node is unconstrained.
    pub low_percent: u32,
    /// At or above this utilization a node stops receiving new replicas.
    pub high_percent: u32,
    /// At or above this utilization the node is treated as critically full.
    pub critical_percent: u32,
}

impl Default for CapacityWatermarks {
    fn default() -> Self {
        Self {
            low_percent: 80,
            high_percent: 90,
            critical_percent: 95,
        }
    }
}

impl CapacityWatermarks {
    /// Classifies a utilization percentage.
    #[must_use]
    pub const fn level(&self, utilization_percent: u32) -> CapacityLevel {
        if utilization_percent >= self.critical_percent {
            CapacityLevel::Critical
        } else if utilization_percent >= self.high_percent {
            CapacityLevel::Urgent
        } else if utilization_percent >= self.low_percent {
            CapacityLevel::Constrained
        } else {
            CapacityLevel::Normal
        }
    }

    fn validate(&self) -> Result<(), ClusterConfigError> {
        if self.low_percent == 0
            || self.low_percent >= self.high_percent
            || self.high_percent >= self.critical_percent
            || self.critical_percent > 100
        {
            return Err(ClusterConfigError(
                "capacity watermarks must satisfy 0 < low < high < critical <= 100".into(),
            ));
        }
        Ok(())
    }
}

/// Capacity pressure classification for one node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityLevel {
    /// Below the low watermark.
    Normal,
    /// Between the low and high watermarks; still writable.
    Constrained,
    /// Between the high and critical watermarks; excluded from new placement.
    Urgent,
    /// At or above the critical watermark; excluded and rebalanced away from.
    Critical,
}

impl CapacityLevel {
    /// Returns whether a node at this level may receive new replicas.
    #[must_use]
    pub const fn accepts_new_replicas(self) -> bool {
        matches!(self, Self::Normal | Self::Constrained)
    }

    /// Returns whether rebalancing should actively drain this node.
    #[must_use]
    pub const fn needs_relief(self) -> bool {
        matches!(self, Self::Urgent | Self::Critical)
    }
}

/// When a replicated write may be acknowledged to the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", content = "replicas", rename_all = "snake_case")]
pub enum WriteAcknowledgement {
    /// A strict majority of the desired replicas must be durable.
    Quorum,
    /// Every desired replica must be durable.
    All,
    /// An explicit number of replicas must be durable.
    Count(u8),
}

impl WriteAcknowledgement {
    /// Resolves the number of durable replicas required for a given factor.
    #[must_use]
    pub const fn required(self, replication_factor: u8) -> u8 {
        match self {
            Self::Quorum => replication_factor / 2 + 1,
            Self::All => replication_factor,
            Self::Count(count) => {
                if count > replication_factor {
                    replication_factor
                } else if count == 0 {
                    1
                } else {
                    count
                }
            }
        }
    }
}

/// Bandwidth and concurrency limits for background replica movement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MovementPolicy {
    /// Maximum replica transfers running cluster-wide.
    pub maximum_concurrent_tasks: u32,
    /// Maximum simultaneous transfer streams involving one node.
    pub maximum_streams_per_node: u32,
    /// Byte-per-second ceiling for one transfer. Zero disables throttling.
    pub maximum_bytes_per_second: u64,
    /// Seconds between scheduling passes.
    pub interval_seconds: u64,
    /// Maximum tasks created in one scheduling pass.
    pub batch_size: u32,
}

impl MovementPolicy {
    fn validate(&self, name: &str) -> Result<(), ClusterConfigError> {
        if self.maximum_concurrent_tasks == 0 || self.maximum_concurrent_tasks > 1_024 {
            return Err(ClusterConfigError(format!(
                "{name}.maximum_concurrent_tasks must be between 1 and 1024"
            )));
        }
        if self.maximum_streams_per_node == 0 || self.maximum_streams_per_node > 256 {
            return Err(ClusterConfigError(format!(
                "{name}.maximum_streams_per_node must be between 1 and 256"
            )));
        }
        if self.interval_seconds == 0 || self.interval_seconds > 86_400 {
            return Err(ClusterConfigError(format!(
                "{name}.interval_seconds must be between 1 and 86400"
            )));
        }
        if self.batch_size == 0 || self.batch_size > 10_000 {
            return Err(ClusterConfigError(format!(
                "{name}.batch_size must be between 1 and 10000"
            )));
        }
        Ok(())
    }
}

/// Repair scheduling limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairPolicy {
    /// Movement limits applied to repair transfers.
    pub movement: MovementPolicy,
    /// Attempts before a repair task is parked for operator attention.
    pub maximum_attempts: u32,
    /// Seconds a claimed repair task remains leased to one node.
    pub lease_seconds: u64,
}

impl Default for RepairPolicy {
    fn default() -> Self {
        Self {
            movement: MovementPolicy {
                maximum_concurrent_tasks: 8,
                maximum_streams_per_node: 4,
                maximum_bytes_per_second: 64 * 1024 * 1024,
                interval_seconds: 30,
                batch_size: 256,
            },
            maximum_attempts: 8,
            lease_seconds: 600,
        }
    }
}

/// Rebalance scheduling limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebalancePolicy {
    /// Whether the coordinator may create rebalance work automatically.
    pub automatic: bool,
    /// Movement limits applied to rebalance transfers.
    pub movement: MovementPolicy,
    /// Utilization spread, in percent, tolerated before moving data.
    pub tolerance_percent: u32,
}

impl Default for RebalancePolicy {
    fn default() -> Self {
        Self {
            automatic: false,
            movement: MovementPolicy {
                maximum_concurrent_tasks: 4,
                maximum_streams_per_node: 2,
                maximum_bytes_per_second: 32 * 1024 * 1024,
                interval_seconds: 300,
                batch_size: 128,
            },
            tolerance_percent: 10,
        }
    }
}

/// Failure-detection timings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureDetectionPolicy {
    /// Interval at which nodes send heartbeats.
    pub heartbeat_interval_seconds: u64,
    /// Silence after which a node becomes `Suspect`.
    pub suspect_timeout_seconds: u64,
    /// Silence after which a node becomes `Offline` and its replicas repaired.
    pub offline_timeout_seconds: u64,
}

impl Default for FailureDetectionPolicy {
    fn default() -> Self {
        Self {
            heartbeat_interval_seconds: 5,
            suspect_timeout_seconds: 20,
            offline_timeout_seconds: 120,
        }
    }
}

impl FailureDetectionPolicy {
    fn validate(&self) -> Result<(), ClusterConfigError> {
        if self.heartbeat_interval_seconds == 0 || self.heartbeat_interval_seconds > 300 {
            return Err(ClusterConfigError(
                "failure_detection.heartbeat_interval_seconds must be between 1 and 300".into(),
            ));
        }
        if self.suspect_timeout_seconds <= self.heartbeat_interval_seconds {
            return Err(ClusterConfigError(
                "failure_detection.suspect_timeout_seconds must exceed the heartbeat interval"
                    .into(),
            ));
        }
        if self.offline_timeout_seconds <= self.suspect_timeout_seconds {
            return Err(ClusterConfigError(
                "failure_detection.offline_timeout_seconds must exceed the suspect timeout".into(),
            ));
        }
        if self.offline_timeout_seconds > 86_400 {
            return Err(ClusterConfigError(
                "failure_detection.offline_timeout_seconds must not exceed 86400".into(),
            ));
        }
        Ok(())
    }
}

/// The replicated cluster-wide configuration document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterConfig {
    /// Desired number of durable replicas per payload.
    pub replication_factor: u8,
    /// Durability required before a write is acknowledged.
    pub write_acknowledgement: WriteAcknowledgement,
    /// Topology scope replicas are spread across.
    pub failure_domain_scope: FailureDomainScope,
    /// Refuse placement that cannot satisfy the failure-domain scope.
    pub strict_failure_domains: bool,
    /// Allow acknowledged writes that reach fewer than `replication_factor`
    /// replicas but still satisfy `write_acknowledgement`.
    pub allow_degraded_writes: bool,
    /// Capacity thresholds.
    pub watermarks: CapacityWatermarks,
    /// Bytes kept free on every node beyond the watermark checks.
    pub capacity_safety_margin_bytes: u64,
    /// Failure-detection timings.
    pub failure_detection: FailureDetectionPolicy,
    /// Repair limits.
    pub repair: RepairPolicy,
    /// Rebalance limits.
    pub rebalance: RebalancePolicy,
    /// Number of consensus voters the cluster aims to maintain.
    pub metadata_voter_target: u8,
    /// Whether nodes in `Maintenance` may still serve reads.
    pub maintenance_serves_reads: bool,
    /// Hours a completed tombstone is retained before it is purged.
    pub tombstone_retention_hours: u32,
    /// Size assumed for placement when a streaming upload has no known length.
    pub unknown_upload_size_reservation_bytes: u64,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            replication_factor: 3,
            write_acknowledgement: WriteAcknowledgement::Quorum,
            failure_domain_scope: FailureDomainScope::Rack,
            strict_failure_domains: false,
            allow_degraded_writes: true,
            watermarks: CapacityWatermarks::default(),
            capacity_safety_margin_bytes: 1024 * 1024 * 1024,
            failure_detection: FailureDetectionPolicy::default(),
            repair: RepairPolicy::default(),
            rebalance: RebalancePolicy::default(),
            metadata_voter_target: 3,
            maintenance_serves_reads: true,
            tombstone_retention_hours: 168,
            unknown_upload_size_reservation_bytes: 64 * 1024 * 1024,
        }
    }
}

impl ClusterConfig {
    /// Returns the configuration used by a standalone single-node deployment.
    #[must_use]
    pub fn standalone() -> Self {
        Self {
            replication_factor: 1,
            write_acknowledgement: WriteAcknowledgement::All,
            failure_domain_scope: FailureDomainScope::Node,
            metadata_voter_target: 1,
            rebalance: RebalancePolicy {
                automatic: false,
                ..RebalancePolicy::default()
            },
            ..Self::default()
        }
    }

    /// Returns the durability required before acknowledging a write.
    #[must_use]
    pub const fn required_acknowledgements(&self) -> u8 {
        self.write_acknowledgement.required(self.replication_factor)
    }

    /// Validates every field and cross-field constraint strictly.
    pub fn validate(&self) -> Result<(), ClusterConfigError> {
        if self.replication_factor == 0 || self.replication_factor > MAXIMUM_REPLICATION_FACTOR {
            return Err(ClusterConfigError(format!(
                "replication_factor must be between 1 and {MAXIMUM_REPLICATION_FACTOR}"
            )));
        }
        if let WriteAcknowledgement::Count(count) = self.write_acknowledgement
            && (count == 0 || count > self.replication_factor)
        {
            return Err(ClusterConfigError(
                "write_acknowledgement count must be between 1 and replication_factor".into(),
            ));
        }
        self.watermarks.validate()?;
        self.failure_detection.validate()?;
        self.repair.movement.validate("repair")?;
        self.rebalance.movement.validate("rebalance")?;
        if self.repair.maximum_attempts == 0 || self.repair.maximum_attempts > 1_000 {
            return Err(ClusterConfigError(
                "repair.maximum_attempts must be between 1 and 1000".into(),
            ));
        }
        if self.repair.lease_seconds < 10 || self.repair.lease_seconds > 86_400 {
            return Err(ClusterConfigError(
                "repair.lease_seconds must be between 10 and 86400".into(),
            ));
        }
        if self.rebalance.tolerance_percent == 0 || self.rebalance.tolerance_percent > 50 {
            return Err(ClusterConfigError(
                "rebalance.tolerance_percent must be between 1 and 50".into(),
            ));
        }
        if self.metadata_voter_target == 0 || self.metadata_voter_target > 7 {
            return Err(ClusterConfigError(
                "metadata_voter_target must be between 1 and 7".into(),
            ));
        }
        if self.metadata_voter_target > 1 && self.metadata_voter_target.is_multiple_of(2) {
            return Err(ClusterConfigError(
                "metadata_voter_target must be odd so that a quorum is unambiguous".into(),
            ));
        }
        if self.tombstone_retention_hours == 0 || self.tombstone_retention_hours > 8_760 {
            return Err(ClusterConfigError(
                "tombstone_retention_hours must be between 1 and 8760".into(),
            ));
        }
        if self.unknown_upload_size_reservation_bytes == 0
            || self.unknown_upload_size_reservation_bytes > 1024_u64.pow(4)
        {
            return Err(ClusterConfigError(
                "unknown_upload_size_reservation_bytes must be between 1 and 1 TiB".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_configuration_is_valid() {
        ClusterConfig::default()
            .validate()
            .expect("default cluster configuration must be valid");
        ClusterConfig::standalone()
            .validate()
            .expect("standalone cluster configuration must be valid");
    }

    #[test]
    fn quorum_acknowledgements_scale_with_replication_factor() {
        assert_eq!(WriteAcknowledgement::Quorum.required(1), 1);
        assert_eq!(WriteAcknowledgement::Quorum.required(2), 2);
        assert_eq!(WriteAcknowledgement::Quorum.required(3), 2);
        assert_eq!(WriteAcknowledgement::All.required(3), 3);
        assert_eq!(WriteAcknowledgement::Count(9).required(3), 3);
        assert_eq!(WriteAcknowledgement::Count(0).required(3), 1);
    }

    #[test]
    fn watermarks_classify_pressure() {
        let watermarks = CapacityWatermarks::default();
        assert_eq!(watermarks.level(10), CapacityLevel::Normal);
        assert_eq!(watermarks.level(85), CapacityLevel::Constrained);
        assert_eq!(watermarks.level(92), CapacityLevel::Urgent);
        assert_eq!(watermarks.level(99), CapacityLevel::Critical);
        assert!(CapacityLevel::Constrained.accepts_new_replicas());
        assert!(!CapacityLevel::Urgent.accepts_new_replicas());
    }

    #[test]
    fn invalid_values_are_refused() {
        let base = ClusterConfig::default();
        let cases = [
            ClusterConfig {
                replication_factor: 0,
                ..base.clone()
            },
            ClusterConfig {
                replication_factor: 4,
                ..base.clone()
            },
            ClusterConfig {
                metadata_voter_target: 2,
                ..base.clone()
            },
            ClusterConfig {
                watermarks: CapacityWatermarks {
                    high_percent: 70,
                    ..base.watermarks
                },
                ..base.clone()
            },
            ClusterConfig {
                failure_detection: FailureDetectionPolicy {
                    offline_timeout_seconds: 5,
                    ..base.failure_detection
                },
                ..base.clone()
            },
            ClusterConfig {
                write_acknowledgement: WriteAcknowledgement::Count(4),
                ..base.clone()
            },
            ClusterConfig {
                tombstone_retention_hours: 0,
                ..base.clone()
            },
            ClusterConfig {
                repair: RepairPolicy {
                    lease_seconds: 1,
                    ..base.repair
                },
                ..base.clone()
            },
        ];
        for case in cases {
            assert!(
                case.validate().is_err(),
                "invalid configuration was accepted: {case:?}"
            );
        }
    }
}
