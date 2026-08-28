//! Configuration loading, environment overrides, secret redaction, and validation.

use std::fmt::{self, Debug, Display, Formatter};

use serde::{Deserialize, Serialize};

use crate::*;

/// How this process participates in a deployment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentMode {
    /// One process owning its own data, with no cluster machinery.
    ///
    /// This remains a first-class deployment: a small installation should not
    /// pay for consensus or replication it does not need.
    #[default]
    Standalone,
    /// A storage node in a cluster: serves S3 traffic and holds replicas.
    Cluster,
    /// A control-plane process: serves the management API and holds no replicas.
    Control,
}

impl DeploymentMode {
    /// Returns whether this process stores object replicas.
    #[must_use]
    pub const fn stores_replicas(self) -> bool {
        matches!(self, Self::Standalone | Self::Cluster)
    }

    /// Returns whether this process serves the S3 API.
    #[must_use]
    pub const fn serves_s3(self) -> bool {
        matches!(self, Self::Standalone | Self::Cluster)
    }

    /// Returns whether this process participates in a cluster.
    #[must_use]
    pub const fn clustered(self) -> bool {
        matches!(self, Self::Cluster | Self::Control)
    }

    /// Returns the stable configuration name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Standalone => "standalone",
            Self::Cluster => "cluster",
            Self::Control => "control",
        }
    }
}

impl Display for DeploymentMode {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::str::FromStr for DeploymentMode {
    type Err = ConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "standalone" => Ok(Self::Standalone),
            "cluster" => Ok(Self::Cluster),
            "control" => Ok(Self::Control),
            other => Err(ConfigError::Validation(format!(
                "unknown deployment mode '{other}'; expected standalone, cluster, or control"
            ))),
        }
    }
}
