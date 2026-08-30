//! Configuration loading, environment overrides, secret redaction, and validation.

use std::{
    fmt::Debug,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    path::PathBuf,
};

use serde::Deserialize;

use crate::*;

/// Listener and shutdown settings.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// How this process participates in a deployment.
    #[serde(default)]
    pub mode: DeploymentMode,
    /// S3-compatible API listener.
    pub s3_bind: SocketAddr,
    /// Native management API listener.
    pub api_bind: SocketAddr,
    /// Internal node-to-node RPC listener.
    ///
    /// This listener is for cluster traffic only and must not be published.
    pub rpc_bind: SocketAddr,
    /// Address peers should use to reach this node's internal listener.
    ///
    /// A bind address is not usable as an advertise address behind Docker,
    /// Kubernetes, or NAT, so the two are configured independently.
    pub rpc_advertise: Option<String>,
    /// Maximum graceful-shutdown drain time.
    pub shutdown_grace_period_seconds: u64,
}

impl ServerConfig {
    /// Port reserved for the future web console. Nothing binds it today.
    pub const RESERVED_CONSOLE_PORT: u16 = 7_602;

    /// Returns the address peers should use for internal RPC.
    ///
    /// Falls back to the bind address, which is only correct when the bind
    /// address is itself routable from peers.
    #[must_use]
    pub fn effective_rpc_advertise(&self) -> String {
        self.rpc_advertise
            .clone()
            .unwrap_or_else(|| self.rpc_bind.to_string())
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            mode: DeploymentMode::Standalone,
            s3_bind: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 7_600)),
            api_bind: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 7_601)),
            rpc_bind: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 7_603)),
            rpc_advertise: None,
            shutdown_grace_period_seconds: 30,
        }
    }
}

/// One additional storage device this node serves.
///
/// A device is a durable location Record Store places data on. Declaring one
/// here is the administrator explicitly choosing it: Record Store never adopts a
/// disk it happens to find, and never formats or claims anything.
///
/// The node's `data_directory` is always a device in its own right, so this list
/// describes the drives *beyond* it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageDeviceConfig {
    /// Stable name for this device on this node.
    ///
    /// Identity is derived from it, so renaming a device is the same as
    /// declaring a different one. It is not the mount path, which can move.
    pub name: String,
    /// Directory this device stores payloads under. Normally a mount point.
    pub path: PathBuf,
    /// Storage class the device belongs to. Defaults to the node's class.
    #[serde(default)]
    pub storage_class: Option<String>,
    /// Placement weight, where 1000 is neutral.
    #[serde(default)]
    pub weight: Option<u32>,
}

/// Durable local-storage locations.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    /// Root of all durable Record Store state.
    pub data_directory: PathBuf,
    /// Optional location for incomplete payload files.
    pub temporary_directory: Option<PathBuf>,
    /// Encrypt newly committed object and multipart payload bytes at rest.
    #[serde(default)]
    pub encryption_enabled: bool,
    /// Additional devices this node serves, beyond `data_directory`.
    #[serde(default)]
    pub devices: Vec<StorageDeviceConfig>,
}

impl StorageConfig {
    /// Returns the explicit temporary directory or `<data_directory>/tmp`.
    #[must_use]
    pub fn effective_temporary_directory(&self) -> PathBuf {
        self.temporary_directory
            .clone()
            .unwrap_or_else(|| self.data_directory.join("tmp"))
    }

    /// Returns where a declared device keeps its incomplete uploads.
    ///
    /// Alongside the device's own payloads, so a part never has to cross devices
    /// to become an object.
    #[must_use]
    pub fn device_temporary_directory(device: &StorageDeviceConfig) -> PathBuf {
        device.path.join("tmp")
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            data_directory: PathBuf::from("./data"),
            temporary_directory: None,
            encryption_enabled: false,
            devices: Vec::new(),
        }
    }
}

/// Credential bootstrap and encryption settings.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Root S3 access key identifier.
    pub root_access_key: Option<String>,
    /// Root S3 secret key.
    pub root_secret_key: Option<SecretValue>,
    /// Stable master key for credentials, webhooks, and optional object encryption.
    pub credential_master_key: Option<SecretValue>,
    /// Whether the bootstrap root credential may authenticate to the S3 API.
    #[serde(default = "default_true")]
    pub root_s3_enabled: bool,
    /// Bearer token granting the full system-administrator management role.
    pub management_system_token: Option<SecretValue>,
    /// Bearer token granting the storage-administrator management role.
    pub management_storage_token: Option<SecretValue>,
    /// Bearer token granting the read-only auditor management role.
    pub management_auditor_token: Option<SecretValue>,
    /// Dedicated bearer token accepted only by the Prometheus scrape endpoint.
    pub metrics_scrape_token: Option<SecretValue>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            root_access_key: None,
            root_secret_key: None,
            credential_master_key: None,
            root_s3_enabled: true,
            management_system_token: None,
            management_storage_token: None,
            management_auditor_token: None,
            metrics_scrape_token: None,
        }
    }
}

/// Bounded request-resource settings.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsConfig {
    /// Maximum simultaneously executing storage operations.
    pub maximum_concurrent_operations: usize,
    /// Maximum number of `x-amz-meta-*` entries on one object.
    pub maximum_custom_metadata_entries: usize,
    /// Maximum aggregate custom-metadata bytes on one object.
    pub maximum_custom_metadata_bytes: usize,
    /// Maximum aggregate HTTP header bytes accepted by the S3 adapter.
    pub maximum_header_bytes: usize,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            maximum_concurrent_operations: 256,
            maximum_custom_metadata_entries: 64,
            maximum_custom_metadata_bytes: 16 * 1024,
            maximum_header_bytes: 64 * 1024,
        }
    }
}

/// Safe-default outbound webhook controls.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookConfig {
    /// Permit plain HTTP endpoints. HTTPS is always permitted.
    pub allow_http: bool,
    /// Permit loopback, private, link-local, and other special-use targets.
    pub allow_private_networks: bool,
    /// Per-attempt network timeout.
    pub request_timeout_seconds: u64,
    /// Total attempts before a delivery becomes permanently failed.
    pub maximum_attempts: u32,
    /// Durable delivery queue polling interval.
    pub poll_interval_seconds: u64,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            allow_http: false,
            allow_private_networks: false,
            request_timeout_seconds: 10,
            maximum_attempts: 6,
            poll_interval_seconds: 2,
        }
    }
}

/// Bounded metadata-driven lifecycle scan controls.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleConfig {
    /// Seconds between lifecycle passes.
    pub interval_seconds: u64,
    /// Maximum current objects and versions scanned per rule and pass.
    pub batch_size: usize,
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        Self {
            interval_seconds: 3_600,
            batch_size: 100,
        }
    }
}

/// Structured logging settings.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservabilityConfig {
    /// `tracing-subscriber` filter expression.
    pub log_filter: String,
    /// Emit newline-delimited JSON when true.
    pub json: bool,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            log_filter: "record_store=info".to_owned(),
            json: false,
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::Config;
    use std::path::PathBuf;

    #[test]
    fn defaults_use_record_store_ports_and_require_credentials() {
        let config = Config::default();
        assert_eq!(config.server.s3_bind.port(), 7_600);
        assert_eq!(config.server.api_bind.port(), 7_601);
        assert!(config.validate().is_err());
    }

    #[test]
    fn temporary_directory_defaults_under_data_root() {
        let mut config = Config::default();
        config.storage.data_directory = PathBuf::from("state");
        assert_eq!(
            config.storage.effective_temporary_directory(),
            PathBuf::from("state/tmp")
        );
    }

    #[test]
    fn default_listeners_use_the_documented_record_store_ports() {
        let server = ServerConfig::default();
        assert_eq!(server.s3_bind.port(), 7_600);
        assert_eq!(server.api_bind.port(), 7_601);
        assert_eq!(server.rpc_bind.port(), 7_603);
        assert_eq!(ServerConfig::RESERVED_CONSOLE_PORT, 7_602);
        for port in [
            server.s3_bind.port(),
            server.api_bind.port(),
            server.rpc_bind.port(),
        ] {
            assert_ne!(
                port, 9_000,
                "Record Store must not default to another product's port"
            );
            assert_ne!(
                port, 9_001,
                "Record Store must not default to another product's port"
            );
        }
        assert_eq!(server.mode, DeploymentMode::Standalone);
        assert_eq!(
            server.effective_rpc_advertise(),
            server.rpc_bind.to_string()
        );
    }
}
