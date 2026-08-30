//! Storage classes as typed policy.
//!
//! A storage class is a name a bucket asks for. A storage policy is what that
//! name means: which devices may hold the data, how durability is achieved, what
//! must be separated from what, and how much room to keep free.
//!
//! Policy is deliberately separate from hardware. `nvme` is a fact about a
//! device; `hot` is a decision about how it is used, and an operator may put any
//! kind of device behind any class. Keeping the two apart is what lets a
//! deployment run entirely on directories without having to lie about what its
//! hardware is.

use std::collections::BTreeSet;

use record_store_core::ErasureProfile;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    device::{DeviceKind, DeviceRecord},
    topology::{FailureDomainScope, StorageClass},
};

/// Largest replication factor a policy may request.
pub const MAXIMUM_POLICY_REPLICAS: u8 = 16;

/// Failures raised while validating a storage policy.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StoragePolicyError {
    /// The replication factor is outside the supported range.
    #[error("replication factor must be between 1 and {MAXIMUM_POLICY_REPLICAS}")]
    InvalidReplicas,
    /// The reserve would leave no usable capacity.
    #[error("minimum free space must be below 100 percent")]
    InvalidReserve,
    /// The requested durability strategy is defined but not yet usable.
    #[error(
        "erasure coding is not available as a bucket durability strategy in this release; \
         the coding engine exists but no write path uses it"
    )]
    ErasureCodingUnavailable,
    /// A policy names a device kind twice.
    #[error("device kind {0:?} is listed more than once")]
    DuplicateDeviceKind(DeviceKind),
    /// The policy does not exist.
    #[error("storage policy '{0}' is not defined")]
    NotFound(StorageClass),
    /// A policy cannot be removed while something still selects it.
    #[error("storage policy '{class}' is still used by {buckets} bucket(s)")]
    StillReferenced {
        /// Class that was asked to be removed.
        class: StorageClass,
        /// How many buckets still name it.
        buckets: usize,
    },
    /// The default policy underpins every unconfigured bucket.
    #[error("the default storage policy cannot be removed")]
    DefaultIsRequired,
}

/// Which devices a class is willing to use.
///
/// An empty allow-list means every kind is acceptable, which is the right
/// default: most deployments do not know or care what the platform reports, and
/// a filter nobody configured should not quietly exclude their storage.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceFilter {
    /// Device kinds this class accepts. Empty accepts anything.
    #[serde(default)]
    pub allowed_kinds: Vec<DeviceKind>,
}

impl DeviceFilter {
    /// Accepts any device kind.
    #[must_use]
    pub const fn any() -> Self {
        Self {
            allowed_kinds: Vec::new(),
        }
    }

    /// Builds a filter from an explicit allow-list.
    pub fn allowing(
        kinds: impl IntoIterator<Item = DeviceKind>,
    ) -> Result<Self, StoragePolicyError> {
        let mut seen = BTreeSet::new();
        let mut allowed_kinds = Vec::new();
        for kind in kinds {
            if !seen.insert(kind) {
                return Err(StoragePolicyError::DuplicateDeviceKind(kind));
            }
            allowed_kinds.push(kind);
        }
        allowed_kinds.sort_unstable();
        Ok(Self { allowed_kinds })
    }

    /// Returns whether the filter accepts a device kind.
    ///
    /// An `Unknown` kind is accepted by an unfiltered policy and rejected by a
    /// filtered one. A platform that could not identify a device is not evidence
    /// that the device is an NVMe drive.
    #[must_use]
    pub fn accepts(&self, kind: DeviceKind) -> bool {
        self.allowed_kinds.is_empty() || self.allowed_kinds.contains(&kind)
    }

    /// Returns whether the filter excludes anything.
    #[must_use]
    pub fn is_unrestricted(&self) -> bool {
        self.allowed_kinds.is_empty()
    }
}

/// How a class achieves durability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "strategy", rename_all = "snake_case")]
pub enum DurabilityStrategy {
    /// Whole-object copies.
    Replication {
        /// Copies the policy wants.
        replicas: u8,
    },
    /// Systematic Reed-Solomon stripes.
    ///
    /// The coding engine exists and is tested, but nothing writes or reads
    /// stripes yet, so a policy using this is refused at validation rather than
    /// silently behaving like replication. The variant exists so the durable
    /// policy format does not have to change when the write path arrives.
    ErasureCoding {
        /// Data and parity shard counts.
        profile: ErasureProfile,
    },
}

impl DurabilityStrategy {
    /// Returns the copies placement should produce, when replicated.
    #[must_use]
    pub const fn replicas(self) -> Option<u8> {
        match self {
            Self::Replication { replicas } => Some(replicas),
            Self::ErasureCoding { .. } => None,
        }
    }
}

impl Default for DurabilityStrategy {
    fn default() -> Self {
        Self::Replication { replicas: 3 }
    }
}

/// What a storage class means.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoragePolicy {
    /// The class label buckets select. This is the policy's identity.
    pub class: StorageClass,
    /// Human-facing description.
    #[serde(default)]
    pub description: Option<String>,
    /// Devices this class may use.
    #[serde(default)]
    pub device_filter: DeviceFilter,
    /// How durability is achieved.
    pub durability: DurabilityStrategy,
    /// Topology level replicas must be separated across.
    pub failure_domain: FailureDomainScope,
    /// Refuse placement that cannot satisfy `failure_domain`.
    #[serde(default)]
    pub strict_failure_domains: bool,
    /// Percentage of a device's usable capacity to keep free.
    #[serde(default)]
    pub minimum_free_space_percent: u8,
}

impl StoragePolicy {
    /// The policy every bucket uses when it selects nothing.
    ///
    /// It has to exist and it has to be unrestricted: buckets created before
    /// storage classes were configurable resolve to it, and narrowing it later
    /// would retroactively change where their data belongs.
    #[must_use]
    pub fn default_policy(failure_domain: FailureDomainScope, replicas: u8) -> Self {
        Self {
            class: StorageClass::default(),
            description: Some("Default policy for buckets that select no class".into()),
            device_filter: DeviceFilter::any(),
            durability: DurabilityStrategy::Replication { replicas },
            failure_domain,
            strict_failure_domains: false,
            minimum_free_space_percent: 0,
        }
    }

    /// Validates the policy and whether it can currently be used.
    pub fn validate(&self) -> Result<(), StoragePolicyError> {
        match self.durability {
            DurabilityStrategy::Replication { replicas } => {
                if replicas == 0 || replicas > MAXIMUM_POLICY_REPLICAS {
                    return Err(StoragePolicyError::InvalidReplicas);
                }
            }
            // Refused rather than accepted-and-ignored: a policy that says
            // "4+2" and stores three copies would be a lie an operator could
            // not detect until they needed the parity.
            DurabilityStrategy::ErasureCoding { .. } => {
                return Err(StoragePolicyError::ErasureCodingUnavailable);
            }
        }
        if self.minimum_free_space_percent >= 100 {
            return Err(StoragePolicyError::InvalidReserve);
        }
        let mut seen = BTreeSet::new();
        for kind in &self.device_filter.allowed_kinds {
            if !seen.insert(*kind) {
                return Err(StoragePolicyError::DuplicateDeviceKind(*kind));
            }
        }
        Ok(())
    }

    /// Returns whether a device may hold data for this class.
    ///
    /// The class label still has to match: a policy describes what a class
    /// means, it does not claim every device in the cluster.
    #[must_use]
    pub fn accepts_device(&self, device: &DeviceRecord) -> bool {
        device.storage_class == self.class && self.device_filter.accepts(device.kind)
    }

    /// Returns the bytes to keep free on a device of the given usable size.
    #[must_use]
    pub fn reserved_bytes(&self, usable_bytes: u64) -> u64 {
        if self.minimum_free_space_percent == 0 {
            return 0;
        }
        u64::try_from(u128::from(usable_bytes) * u128::from(self.minimum_free_space_percent) / 100)
            .unwrap_or(u64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> StoragePolicy {
        StoragePolicy::default_policy(FailureDomainScope::Node, 3)
    }

    /// A class nobody restricted must not quietly exclude a deployment's
    /// storage, and one that was restricted must not be widened by a device the
    /// platform failed to identify.
    #[test]
    fn an_unfiltered_class_accepts_anything_and_a_filtered_one_does_not() {
        let unrestricted = DeviceFilter::any();
        assert!(unrestricted.accepts(DeviceKind::Unknown));
        assert!(unrestricted.accepts(DeviceKind::Nvme));

        let solid_state =
            DeviceFilter::allowing([DeviceKind::Nvme, DeviceKind::SataSsd]).expect("filter");
        assert!(solid_state.accepts(DeviceKind::Nvme));
        assert!(!solid_state.accepts(DeviceKind::SataHdd));
        assert!(
            !solid_state.accepts(DeviceKind::Unknown),
            "an unidentified device must not satisfy an explicit hardware requirement"
        );
    }

    #[test]
    fn a_duplicated_device_kind_is_refused() {
        assert!(matches!(
            DeviceFilter::allowing([DeviceKind::Nvme, DeviceKind::Nvme]),
            Err(StoragePolicyError::DuplicateDeviceKind(DeviceKind::Nvme))
        ));
    }

    /// Erasure coding is expressible so the durable format is ready for it, and
    /// refused so no bucket can select a durability Record Store cannot deliver.
    #[test]
    fn an_erasure_coded_policy_is_refused_rather_than_silently_replicated() {
        let mut policy = policy();
        policy.durability = DurabilityStrategy::ErasureCoding {
            profile: ErasureProfile::new(4, 2).expect("profile"),
        };
        assert!(matches!(
            policy.validate(),
            Err(StoragePolicyError::ErasureCodingUnavailable)
        ));
        assert_eq!(policy.durability.replicas(), None);
    }

    #[test]
    fn replica_counts_and_reserves_are_bounded() {
        let mut policy = policy();
        policy.durability = DurabilityStrategy::Replication { replicas: 0 };
        assert!(matches!(
            policy.validate(),
            Err(StoragePolicyError::InvalidReplicas)
        ));

        policy.durability = DurabilityStrategy::Replication {
            replicas: MAXIMUM_POLICY_REPLICAS + 1,
        };
        assert!(matches!(
            policy.validate(),
            Err(StoragePolicyError::InvalidReplicas)
        ));

        policy.durability = DurabilityStrategy::Replication { replicas: 3 };
        policy.minimum_free_space_percent = 100;
        assert!(matches!(
            policy.validate(),
            Err(StoragePolicyError::InvalidReserve)
        ));

        policy.minimum_free_space_percent = 15;
        policy.validate().expect("a bounded policy is valid");
        assert_eq!(policy.reserved_bytes(1_000), 150);
        assert_eq!(policy.reserved_bytes(0), 0);
    }

    /// The default policy is what every unconfigured bucket resolves to, so it
    /// must never exclude a device on hardware grounds.
    #[test]
    fn the_default_policy_is_unrestricted_and_valid() {
        let policy = policy();
        policy.validate().expect("valid");
        assert!(policy.device_filter.is_unrestricted());
        assert_eq!(policy.class, StorageClass::default());
        assert_eq!(policy.durability.replicas(), Some(3));
    }
}
