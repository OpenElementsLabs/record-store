//! Internal protocol, software, and storage-format compatibility rules.
//!
//! Every internal connection negotiates these values before any cluster state
//! is exchanged. A node whose versions are incompatible is refused with an
//! actionable administrative error instead of silently joining.

use std::fmt::{self, Display, Formatter};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Major version of the internal node-to-node protocol.
///
/// Incrementing this value declares a breaking wire change. Nodes only ever
/// interoperate within one major version.
pub const PROTOCOL_MAJOR_VERSION: u32 = 1;

/// Minor version of the internal node-to-node protocol.
///
/// Increments are additive: a newer minor version understands every request an
/// older minor version can send.
pub const PROTOCOL_MINOR_VERSION: u32 = 1;

/// Oldest minor version this build still accepts within the same major version.
///
/// This is the supported rolling-upgrade compatibility window.
pub const MINIMUM_COMPATIBLE_PROTOCOL_MINOR_VERSION: u32 = 1;

/// Durable on-disk layout version of a storage node's replica directory.
pub const STORAGE_FORMAT_VERSION: u32 = 1;

/// Durable layout version of the replicated cluster catalog.
///
/// Version 3 adds storage policies. A binary that does not understand them
/// would resolve every bucket to the cluster defaults instead of to its
/// configured class, which is a placement difference an operator could not see,
/// so an older binary is refused rather than allowed to diverge.
pub const CLUSTER_FORMAT_VERSION: u32 = 3;

/// A negotiated internal protocol version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ProtocolVersion {
    /// Breaking wire-format generation.
    pub major: u32,
    /// Additive revision inside a major generation.
    pub minor: u32,
}

impl ProtocolVersion {
    /// Returns the protocol version implemented by this build.
    #[must_use]
    pub const fn current() -> Self {
        Self {
            major: PROTOCOL_MAJOR_VERSION,
            minor: PROTOCOL_MINOR_VERSION,
        }
    }

    /// Creates an explicit protocol version.
    #[must_use]
    pub const fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }
}

impl Display for ProtocolVersion {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}", self.major, self.minor)
    }
}

/// The full version tuple a node advertises during a handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeVersions {
    /// Internal protocol version.
    pub protocol: ProtocolVersion,
    /// Human-readable build version, for operators and metrics only.
    pub software: String,
    /// Durable replica-directory layout version.
    pub storage_format: u32,
    /// Durable cluster-catalog layout version.
    pub cluster_format: u32,
}

impl NodeVersions {
    /// Returns the versions implemented by this build.
    #[must_use]
    pub fn current(software: impl Into<String>) -> Self {
        Self {
            protocol: ProtocolVersion::current(),
            software: software.into(),
            storage_format: STORAGE_FORMAT_VERSION,
            cluster_format: CLUSTER_FORMAT_VERSION,
        }
    }
}

/// Reasons a peer is refused before it can affect cluster state.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CompatibilityError {
    /// The peer speaks a different breaking protocol generation.
    #[error(
        "incompatible internal protocol: local major {local}, remote major {remote}; \
         upgrade both nodes to the same major protocol version"
    )]
    ProtocolMajor {
        /// Local major version.
        local: u32,
        /// Remote major version.
        remote: u32,
    },
    /// The peer is older than this build's supported compatibility window.
    #[error(
        "internal protocol minor version {remote} is older than the supported window \
         (minimum {minimum}); upgrade the remote node"
    )]
    ProtocolMinorTooOld {
        /// Remote minor version.
        remote: u32,
        /// Oldest accepted minor version.
        minimum: u32,
    },
    /// The peer wrote a newer durable replica layout than this build understands.
    #[error(
        "remote storage format version {remote} is newer than this build supports ({local}); \
         upgrade this node before it reads that data"
    )]
    StorageFormatTooNew {
        /// Local supported version.
        local: u32,
        /// Remote version.
        remote: u32,
    },
    /// The peer wrote a newer durable cluster catalog than this build understands.
    #[error(
        "remote cluster catalog format version {remote} is newer than this build supports \
         ({local}); upgrade this node before it joins"
    )]
    ClusterFormatTooNew {
        /// Local supported version.
        local: u32,
        /// Remote version.
        remote: u32,
    },
}

/// Validates that a peer may participate in this cluster.
///
/// Newer peers within the same major protocol version are accepted, because
/// additive minor revisions are backward compatible. Durable formats are only
/// accepted when this build can read them.
pub fn check_compatibility(
    local: &NodeVersions,
    remote: &NodeVersions,
) -> Result<(), CompatibilityError> {
    if local.protocol.major != remote.protocol.major {
        return Err(CompatibilityError::ProtocolMajor {
            local: local.protocol.major,
            remote: remote.protocol.major,
        });
    }
    if remote.protocol.minor < MINIMUM_COMPATIBLE_PROTOCOL_MINOR_VERSION {
        return Err(CompatibilityError::ProtocolMinorTooOld {
            remote: remote.protocol.minor,
            minimum: MINIMUM_COMPATIBLE_PROTOCOL_MINOR_VERSION,
        });
    }
    if remote.storage_format > local.storage_format {
        return Err(CompatibilityError::StorageFormatTooNew {
            local: local.storage_format,
            remote: remote.storage_format,
        });
    }
    if remote.cluster_format > local.cluster_format {
        return Err(CompatibilityError::ClusterFormatTooNew {
            local: local.cluster_format,
            remote: remote.cluster_format,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn versions(major: u32, minor: u32, storage: u32, cluster: u32) -> NodeVersions {
        NodeVersions {
            protocol: ProtocolVersion::new(major, minor),
            software: "test".into(),
            storage_format: storage,
            cluster_format: cluster,
        }
    }

    #[test]
    fn identical_versions_are_compatible() {
        let local = NodeVersions::current("0.1.0");
        assert!(check_compatibility(&local, &local).is_ok());
    }

    #[test]
    fn major_mismatch_is_refused() {
        let local = versions(1, 1, 1, 1);
        let remote = versions(2, 1, 1, 1);
        assert!(matches!(
            check_compatibility(&local, &remote),
            Err(CompatibilityError::ProtocolMajor { .. })
        ));
    }

    #[test]
    fn newer_minor_peers_are_accepted_and_older_windows_are_refused() {
        let local = versions(1, 1, 1, 1);
        assert!(check_compatibility(&local, &versions(1, 9, 1, 1)).is_ok());
        assert!(matches!(
            check_compatibility(&local, &versions(1, 0, 1, 1)),
            Err(CompatibilityError::ProtocolMinorTooOld { .. })
        ));
    }

    #[test]
    fn newer_durable_formats_are_refused_in_both_dimensions() {
        let local = versions(1, 1, 1, 1);
        assert!(matches!(
            check_compatibility(&local, &versions(1, 1, 2, 1)),
            Err(CompatibilityError::StorageFormatTooNew { .. })
        ));
        assert!(matches!(
            check_compatibility(&local, &versions(1, 1, 1, 2)),
            Err(CompatibilityError::ClusterFormatTooNew { .. })
        ));
    }
}
