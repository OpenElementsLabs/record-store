use serde::{Deserialize, Serialize};

use crate::*;

/// Validated systematic Reed-Solomon profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ErasureProfileWire", into = "ErasureProfileWire")]
pub struct ErasureProfile {
    data_shards: u8,
    parity_shards: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ErasureProfileWire {
    data_shards: u8,
    parity_shards: u8,
}

impl ErasureProfile {
    /// Maximum total shard count in storage format version 1.
    pub const MAX_TOTAL_SHARDS: u8 = 32;

    /// Creates a profile with `K` data shards and `M` parity shards.
    pub fn new(data_shards: u8, parity_shards: u8) -> Result<Self, CoreError> {
        if data_shards == 0 {
            return Err(CoreError::InvalidErasureProfile(
                "data_shards must be at least 1".into(),
            ));
        }
        if parity_shards == 0 {
            return Err(CoreError::InvalidErasureProfile(
                "parity_shards must be at least 1".into(),
            ));
        }
        let total = data_shards.checked_add(parity_shards).ok_or_else(|| {
            CoreError::InvalidErasureProfile("total shard count overflows".into())
        })?;
        if total > Self::MAX_TOTAL_SHARDS {
            return Err(CoreError::InvalidErasureProfile(format!(
                "at most {} total shards are supported",
                Self::MAX_TOTAL_SHARDS
            )));
        }
        Ok(Self {
            data_shards,
            parity_shards,
        })
    }

    /// Number of original-data shards (`K`).
    #[must_use]
    pub const fn data_shards(self) -> u8 {
        self.data_shards
    }

    /// Number of parity shards (`M`).
    #[must_use]
    pub const fn parity_shards(self) -> u8 {
        self.parity_shards
    }

    /// Total shards stored for each stripe.
    #[must_use]
    pub const fn total_shards(self) -> u8 {
        self.data_shards + self.parity_shards
    }

    /// Returns the role for a validated index.
    #[must_use]
    pub const fn kind(self, index: ShardIndex) -> ShardKind {
        if index.get() < self.data_shards as u16 {
            ShardKind::Data
        } else {
            ShardKind::Parity
        }
    }
}

impl TryFrom<ErasureProfileWire> for ErasureProfile {
    type Error = CoreError;

    fn try_from(value: ErasureProfileWire) -> Result<Self, Self::Error> {
        Self::new(value.data_shards, value.parity_shards)
    }
}

impl From<ErasureProfile> for ErasureProfileWire {
    fn from(value: ErasureProfile) -> Self {
        Self {
            data_shards: value.data_shards,
            parity_shards: value.parity_shards,
        }
    }
}

/// The actual replica count and acknowledgement threshold used by one payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ReplicationProfileWire", into = "ReplicationProfileWire")]
pub struct ReplicationProfile {
    replicas: u8,
    required_acknowledgements: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReplicationProfileWire {
    replicas: u8,
    required_acknowledgements: u8,
}

impl ReplicationProfile {
    /// Creates a validated replication profile.
    pub fn new(replicas: u8, required_acknowledgements: u8) -> Result<Self, CoreError> {
        if replicas == 0 {
            return Err(CoreError::InvalidReplicationProfile(
                "replicas must be at least 1".into(),
            ));
        }
        if required_acknowledgements == 0 || required_acknowledgements > replicas {
            return Err(CoreError::InvalidReplicationProfile(
                "required acknowledgements must be between 1 and replicas".into(),
            ));
        }
        Ok(Self {
            replicas,
            required_acknowledgements,
        })
    }

    /// Desired replica count.
    #[must_use]
    pub const fn replicas(self) -> u8 {
        self.replicas
    }

    /// Required verified acknowledgements.
    #[must_use]
    pub const fn required_acknowledgements(self) -> u8 {
        self.required_acknowledgements
    }
}

impl TryFrom<ReplicationProfileWire> for ReplicationProfile {
    type Error = CoreError;

    fn try_from(value: ReplicationProfileWire) -> Result<Self, Self::Error> {
        Self::new(value.replicas, value.required_acknowledgements)
    }
}

impl From<ReplicationProfile> for ReplicationProfileWire {
    fn from(value: ReplicationProfile) -> Self {
        Self {
            replicas: value.replicas,
            required_acknowledgements: value.required_acknowledgements,
        }
    }
}

/// Physical durability actually committed for one immutable object version.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "strategy", content = "profile", rename_all = "snake_case")]
pub enum DurabilityProfile {
    /// One local copy. This remains the standalone default.
    #[default]
    Single,
    /// Full-object replication.
    Replicated(ReplicationProfile),
    /// Systematic Reed-Solomon stripes.
    ErasureCoded(ErasureProfile),
}
