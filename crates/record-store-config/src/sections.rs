//! Configuration loading, environment overrides, secret redaction, and validation.

use std::{
    fmt::Debug,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    path::PathBuf,
};

use record_store_core::{CoreError, TrustedProxies};
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
    /// How long a client may take to send a request's headers, and how long an
    /// idle keep-alive connection waits for the next request, before both
    /// listeners close the connection. Without a bound, a client that trickles
    /// a header line at a time holds a connection -- a descriptor and a task --
    /// for as long as it likes.
    #[serde(default = "default_header_read_timeout_seconds")]
    pub header_read_timeout_seconds: u64,
    /// Reverse-proxy hops whose `X-Forwarded-For` header may be believed.
    ///
    /// Addresses or CIDR blocks, for example `10.0.0.0/8`. Empty by default,
    /// which means the header is ignored and every request is attributed to
    /// the socket it arrived on. That is the safe default and the wrong one
    /// behind a proxy: until a hop is named here, every visitor arriving
    /// through it shares one identity, and abuse controls and audit records
    /// say the proxy's address rather than the caller's.
    ///
    /// Naming a hop that is not actually in front of Record Store hands
    /// anybody who can reach the listener from that address the ability to
    /// choose their own identity, so the list should contain the proxy and
    /// nothing else.
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
}

impl ServerConfig {
    /// Port reserved for the future web console. Nothing binds it today.
    pub const RESERVED_CONSOLE_PORT: u16 = 7_602;

    /// Returns the parsed trusted-proxy policy.
    ///
    /// Parsing here rather than at use time means a malformed entry is a
    /// start-up failure, not a silent decision to trust nothing.
    pub fn parsed_trusted_proxies(&self) -> Result<TrustedProxies, CoreError> {
        TrustedProxies::parse(&self.trusted_proxies)
    }

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

const fn default_header_read_timeout_seconds() -> u64 {
    30
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
            header_read_timeout_seconds: default_header_read_timeout_seconds(),
            trusted_proxies: Vec::new(),
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
    /// Movement transfers this device runs at once.
    ///
    /// Omitted derives it from the hardware: one for rotational media, more for
    /// solid state. Set it when you have measured something better.
    #[serde(default)]
    pub movement_concurrency: Option<u32>,
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
    /// Page cache, in MiB, shared by the catalog, the audit trail and the
    /// event journal: half for the catalog, a quarter each for the other two.
    /// The credential, sharing and lifecycle databases, which stay small, keep
    /// a fixed 16 MiB each. The cache fills as the databases grow and is never
    /// larger than this, so it is the part of the server's memory that scales
    /// with history rather than with load.
    #[serde(default = "default_metadata_cache_mib")]
    pub metadata_cache_mib: u64,
}

const fn default_metadata_cache_mib() -> u64 {
    128
}

/// How `storage.metadata_cache_mib` is divided between the databases that grow
/// with use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetadataCacheBudget {
    pub catalog_bytes: usize,
    pub audit_bytes: usize,
    pub events_bytes: usize,
}

impl StorageConfig {
    /// Splits the configured metadata cache: the catalog serves every read and
    /// write, while the audit trail and event journal are appended to and read
    /// near their heads, so they need less of it.
    #[must_use]
    pub fn metadata_cache_budget(&self) -> MetadataCacheBudget {
        let total = usize::try_from(self.metadata_cache_mib.saturating_mul(1024 * 1024))
            .unwrap_or(usize::MAX);
        MetadataCacheBudget {
            catalog_bytes: total / 2,
            audit_bytes: total / 4,
            events_bytes: total / 4,
        }
    }

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
            metadata_cache_mib: default_metadata_cache_mib(),
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
    /// How long an operation may wait for a concurrency permit before it is
    /// refused with a retryable "slow down" rather than queued.
    ///
    /// The concurrency limit bounds the work in flight. This bounds the work
    /// waiting to be in flight, which is what otherwise grows without limit
    /// under sustained overload.
    pub admission_wait_limit_seconds: u32,
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
            // Long enough that an ordinary burst queues and clears, short
            // enough that a client learns the deployment is saturated while the
            // answer is still useful to it.
            admission_wait_limit_seconds: 15,
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

/// Object Lock clock settings.
///
/// A retention date is only as trustworthy as the clock that judges it, so
/// Record Store remembers the furthest point in time it has ever observed and
/// stops releasing retained versions when the clock falls behind it. These two
/// settings decide how often that mark is refreshed and how much ordinary drift
/// is tolerated before the refusal kicks in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectLockConfig {
    /// Seconds between refreshes of the observed-time high-water mark.
    ///
    /// This is what lets an idle deployment still notice a clock that went
    /// backwards while nothing was being written.
    pub clock_watermark_interval_seconds: u64,
    /// Seconds the clock may lag the high-water mark before retention stops
    /// being released. Ordinary NTP correction fits inside this; a jump does not.
    pub clock_backwards_tolerance_seconds: u32,
}

impl Default for ObjectLockConfig {
    fn default() -> Self {
        Self {
            clock_watermark_interval_seconds: 60,
            clock_backwards_tolerance_seconds: 5,
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
