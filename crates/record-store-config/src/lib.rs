//! Configuration loading, environment overrides, secret redaction, and validation.

use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    fmt::{self, Debug, Display, Formatter},
    fs,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

const fn default_true() -> bool {
    true
}

/// A configuration secret whose debug representation is always redacted.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct SecretValue(String);

impl SecretValue {
    /// Constructs a secret from a trusted configuration source.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Exposes the secret only to code that must use it cryptographically.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl Debug for SecretValue {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// Fully resolved and validated OES configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Listener and lifecycle settings.
    pub server: ServerConfig,
    /// Local storage settings.
    pub storage: StorageConfig,
    /// Root credentials and injected master-key settings.
    pub auth: AuthConfig,
    /// Request and concurrency limits.
    pub limits: LimitsConfig,
    /// Outbound webhook safety and retry settings.
    pub webhooks: WebhookConfig,
    /// Incremental object expiration settings.
    pub lifecycle: LifecycleConfig,
    /// Share-link and embed-link policy.
    pub sharing: SharingConfig,
    /// Node-local cluster settings.
    pub cluster: ClusterConfig,
    /// Logging settings.
    pub observability: ObservabilityConfig,
}

impl Config {
    /// Loads defaults, overlays an optional TOML file, overlays `OES_*`
    /// environment variables, and validates the result.
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        Self::load_with_environment(path, env::vars_os())
    }

    /// Loads configuration using an explicit environment source.
    pub fn load_with_environment<I, K, V>(
        path: Option<&Path>,
        environment: I,
    ) -> Result<Self, ConfigError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<OsString>,
        V: Into<OsString>,
    {
        let mut config = match path {
            Some(path) => {
                let contents =
                    fs::read_to_string(path).map_err(|source| ConfigError::ReadFile {
                        path: path.to_path_buf(),
                        source,
                    })?;
                let partial: PartialConfig =
                    toml::from_str(&contents).map_err(|source| ConfigError::ParseFile {
                        path: path.to_path_buf(),
                        source,
                    })?;
                partial.apply(Self::default())
            }
            None => Self::default(),
        };
        let environment = environment
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect::<HashMap<_, _>>();
        config.apply_environment(&environment)?;
        config.validate()?;
        Ok(config)
    }

    /// Returns the address embed links are published on.
    ///
    /// Embeds serve object bytes, so they belong on the storage endpoint rather
    /// than the console. The explicit setting wins; the advertised S3 endpoint
    /// is the next best answer a deployment has already given; and the listener
    /// address is the last resort, correct for a local install and wrong the
    /// moment anything sits in front of it.
    #[must_use]
    pub fn effective_embed_base_url(&self) -> String {
        if let Some(configured) = self.sharing.normalized_embed_base_url() {
            return configured;
        }
        if let Some(endpoint) = self
            .cluster
            .s3_endpoint
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let endpoint = endpoint.trim_end_matches('/');
            return if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
                endpoint.to_owned()
            } else {
                format!("http://{endpoint}")
            };
        }
        // An unspecified bind address is reachable from nowhere, so it is
        // rendered as loopback: a link that works locally is more useful than
        // one naming an address no client can resolve.
        let bind = self.server.s3_bind;
        if bind.ip().is_unspecified() {
            format!("http://127.0.0.1:{}", bind.port())
        } else {
            format!("http://{bind}")
        }
    }

    /// Validates cross-field and security-sensitive constraints.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut issues = Vec::new();
        if self.server.s3_bind.port() == 0 {
            issues.push("server.s3_bind port must be greater than zero".to_owned());
        }
        if self.server.api_bind.port() == 0 {
            issues.push("server.api_bind port must be greater than zero".to_owned());
        }
        if self.server.rpc_bind.port() == 0 {
            issues.push("server.rpc_bind port must be greater than zero".to_owned());
        }
        let listeners = [
            ("server.s3_bind", self.server.s3_bind),
            ("server.api_bind", self.server.api_bind),
            ("server.rpc_bind", self.server.rpc_bind),
        ];
        for left in 0..listeners.len() {
            for right in left + 1..listeners.len() {
                if listeners[left].1 == listeners[right].1 {
                    issues.push(format!(
                        "{} and {} must be different",
                        listeners[left].0, listeners[right].0
                    ));
                }
            }
        }
        for (name, address) in listeners {
            if address.port() == ServerConfig::RESERVED_CONSOLE_PORT {
                issues.push(format!(
                    "{name} must not use port {}, which is reserved for the web console",
                    ServerConfig::RESERVED_CONSOLE_PORT
                ));
            }
        }
        if self
            .server
            .rpc_advertise
            .as_ref()
            .is_some_and(|value| value.trim().is_empty() || value.len() > 253)
        {
            issues.push(
                "server.rpc_advertise must be a non-empty host:port under 253 bytes".to_owned(),
            );
        }
        issues.extend(self.cluster.issues(self.server.mode));
        if !(1..=300).contains(&self.server.shutdown_grace_period_seconds) {
            issues
                .push("server.shutdown_grace_period_seconds must be between 1 and 300".to_owned());
        }
        if self.storage.data_directory.as_os_str().is_empty() {
            issues.push("storage.data_directory must not be empty".to_owned());
        }
        if self
            .storage
            .temporary_directory
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            issues.push("storage.temporary_directory must not be empty".to_owned());
        }
        if self.storage.encryption_enabled && self.auth.credential_master_key.is_none() {
            issues.push(
                "auth.credential_master_key is required when storage.encryption_enabled is true"
                    .to_owned(),
            );
        }
        match (&self.auth.root_access_key, &self.auth.root_secret_key) {
            (Some(access), Some(secret)) => {
                if !(3..=128).contains(&access.len())
                    || !access.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                    })
                {
                    issues.push(
                        "auth.root_access_key must contain 3 to 128 ASCII letters, digits, hyphens, underscores, or periods"
                            .to_owned(),
                    );
                }
                if !(16..=256).contains(&secret.expose().len())
                    || !secret.expose().bytes().all(|byte| byte.is_ascii_graphic())
                {
                    issues.push(
                        "auth.root_secret_key must contain 16 to 256 visible ASCII characters"
                            .to_owned(),
                    );
                }
            }
            (None, None) => issues.push(
                "root credentials are required; set OES_ROOT_ACCESS_KEY and OES_ROOT_SECRET_KEY"
                    .to_owned(),
            ),
            _ => issues.push(
                "auth.root_access_key and auth.root_secret_key must be configured together"
                    .to_owned(),
            ),
        }
        if self.auth.credential_master_key.as_ref().is_some_and(|key| {
            !(32..=1024).contains(&key.expose().len())
                || !key.expose().bytes().all(|byte| byte.is_ascii_graphic())
        }) {
            issues.push(
                "auth.credential_master_key must contain 32 to 1024 visible ASCII characters"
                    .to_owned(),
            );
        }
        for (name, token) in [
            (
                "auth.management_system_token",
                &self.auth.management_system_token,
            ),
            (
                "auth.management_storage_token",
                &self.auth.management_storage_token,
            ),
            (
                "auth.management_auditor_token",
                &self.auth.management_auditor_token,
            ),
            ("auth.metrics_scrape_token", &self.auth.metrics_scrape_token),
        ] {
            if token.as_ref().is_some_and(|value| {
                !(32..=1024).contains(&value.expose().len())
                    || !value.expose().bytes().all(|byte| byte.is_ascii_graphic())
            }) {
                issues.push(format!(
                    "{name} must contain 32 to 1024 visible ASCII characters"
                ));
            }
        }
        if self.auth.management_system_token.is_none()
            && (self.auth.management_storage_token.is_some()
                || self.auth.management_auditor_token.is_some())
        {
            issues.push(
                "auth.management_system_token is required when another management role token is configured"
                    .to_owned(),
            );
        }
        let management_tokens = [
            self.auth.management_system_token.as_ref(),
            self.auth.management_storage_token.as_ref(),
            self.auth.management_auditor_token.as_ref(),
        ];
        for left in 0..management_tokens.len() {
            for right in left + 1..management_tokens.len() {
                if management_tokens[left].is_some()
                    && management_tokens[left] == management_tokens[right]
                {
                    issues.push("management role tokens must be distinct".to_owned());
                }
            }
        }
        if let Some(metrics_token) = &self.auth.metrics_scrape_token
            && management_tokens
                .iter()
                .flatten()
                .any(|management_token| *management_token == metrics_token)
        {
            issues.push(
                "auth.metrics_scrape_token must be distinct from management role tokens".to_owned(),
            );
        }
        if self.limits.maximum_concurrent_operations == 0 {
            issues
                .push("limits.maximum_concurrent_operations must be greater than zero".to_owned());
        }
        if self.limits.maximum_custom_metadata_entries > 1_024 {
            issues.push("limits.maximum_custom_metadata_entries must not exceed 1024".to_owned());
        }
        if self.limits.maximum_custom_metadata_bytes == 0
            || self.limits.maximum_custom_metadata_bytes > 1024 * 1024
        {
            issues.push(
                "limits.maximum_custom_metadata_bytes must be between 1 and 1048576".to_owned(),
            );
        }
        if !(1_024..=1024 * 1024).contains(&self.limits.maximum_header_bytes) {
            issues.push("limits.maximum_header_bytes must be between 1024 and 1048576".to_owned());
        }
        if self.webhooks.request_timeout_seconds == 0 || self.webhooks.request_timeout_seconds > 300
        {
            issues.push("webhooks.request_timeout_seconds must be between 1 and 300".to_owned());
        }
        if self.webhooks.maximum_attempts == 0 || self.webhooks.maximum_attempts > 32 {
            issues.push("webhooks.maximum_attempts must be between 1 and 32".to_owned());
        }
        if self.webhooks.poll_interval_seconds == 0 || self.webhooks.poll_interval_seconds > 3600 {
            issues.push("webhooks.poll_interval_seconds must be between 1 and 3600".to_owned());
        }
        if self.lifecycle.interval_seconds == 0 || self.lifecycle.interval_seconds > 86_400 {
            issues.push("lifecycle.interval_seconds must be between 1 and 86400".to_owned());
        }
        if self.lifecycle.batch_size == 0 || self.lifecycle.batch_size > 1_000 {
            issues.push("lifecycle.batch_size must be between 1 and 1000".to_owned());
        }
        issues.extend(self.sharing.issues());
        if self.observability.log_filter.trim().is_empty() {
            issues.push("observability.log_filter must not be empty".to_owned());
        }
        if issues.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Validation(issues.join("; ")))
        }
    }

    /// Returns configured root credentials after validation.
    pub fn root_credentials(&self) -> Result<(&str, &SecretValue), ConfigError> {
        self.auth
            .root_access_key
            .as_deref()
            .zip(self.auth.root_secret_key.as_ref())
            .ok_or_else(|| ConfigError::Validation("root credentials are required".into()))
    }

    fn apply_environment(
        &mut self,
        environment: &HashMap<OsString, OsString>,
    ) -> Result<(), ConfigError> {
        if let Some(value) = environment_value(environment, "OES_MODE")? {
            self.server.mode = value.parse()?;
        }
        if let Some(value) = environment_value(environment, "OES_S3_BIND")? {
            self.server.s3_bind = parse_environment("OES_S3_BIND", value)?;
        }
        if let Some(value) = environment_value(environment, "OES_RPC_BIND")? {
            self.server.rpc_bind = parse_environment("OES_RPC_BIND", value)?;
        }
        if let Some(value) = environment_value(environment, "OES_RPC_ADVERTISE")? {
            self.server.rpc_advertise = Some(value.to_owned());
        }
        if let Some(value) = environment_value(environment, "OES_API_BIND")? {
            self.server.api_bind = parse_environment("OES_API_BIND", value)?;
        }
        if let Some(value) = environment_value(environment, "OES_SHUTDOWN_TIMEOUT_SECONDS")? {
            self.server.shutdown_grace_period_seconds =
                parse_environment("OES_SHUTDOWN_TIMEOUT_SECONDS", value)?;
        }
        if let Some(value) = environment_value(environment, "OES_STORAGE_DATA_DIRECTORY")? {
            self.storage.data_directory = PathBuf::from(value);
        }
        if let Some(value) = environment_value(environment, "OES_STORAGE_TEMPORARY_DIRECTORY")? {
            self.storage.temporary_directory = Some(PathBuf::from(value));
        }
        if let Some(value) = environment_value(environment, "OES_STORAGE_ENCRYPTION_ENABLED")? {
            self.storage.encryption_enabled =
                parse_environment("OES_STORAGE_ENCRYPTION_ENABLED", value)?;
        }
        if let Some(value) = environment_value(environment, "OES_ROOT_ACCESS_KEY")? {
            self.auth.root_access_key = Some(value.to_owned());
        }
        if let Some(value) = environment_value(environment, "OES_ROOT_SECRET_KEY")? {
            self.auth.root_secret_key = Some(SecretValue::new(value));
        }
        if let Some(value) = environment_value(environment, "OES_CREDENTIAL_MASTER_KEY")? {
            self.auth.credential_master_key = Some(SecretValue::new(value));
        }
        if let Some(value) = environment_value(environment, "OES_ROOT_S3_ENABLED")? {
            self.auth.root_s3_enabled = parse_environment("OES_ROOT_S3_ENABLED", value)?;
        }
        if let Some(value) = environment_value(environment, "OES_MANAGEMENT_SYSTEM_TOKEN")? {
            self.auth.management_system_token = Some(SecretValue::new(value));
        }
        if let Some(value) = environment_value(environment, "OES_MANAGEMENT_STORAGE_TOKEN")? {
            self.auth.management_storage_token = Some(SecretValue::new(value));
        }
        if let Some(value) = environment_value(environment, "OES_MANAGEMENT_AUDITOR_TOKEN")? {
            self.auth.management_auditor_token = Some(SecretValue::new(value));
        }
        if let Some(value) = environment_value(environment, "OES_METRICS_SCRAPE_TOKEN")? {
            self.auth.metrics_scrape_token = Some(SecretValue::new(value));
        }
        if let Some(value) = environment_value(environment, "OES_MAX_CONCURRENT_OPERATIONS")? {
            self.limits.maximum_concurrent_operations =
                parse_environment("OES_MAX_CONCURRENT_OPERATIONS", value)?;
        }
        if let Some(value) = environment_value(environment, "OES_MAX_HEADER_BYTES")? {
            self.limits.maximum_header_bytes = parse_environment("OES_MAX_HEADER_BYTES", value)?;
        }
        if let Some(value) = environment_value(environment, "OES_WEBHOOK_ALLOW_HTTP")? {
            self.webhooks.allow_http = parse_environment("OES_WEBHOOK_ALLOW_HTTP", value)?;
        }
        if let Some(value) = environment_value(environment, "OES_WEBHOOK_ALLOW_PRIVATE_NETWORKS")? {
            self.webhooks.allow_private_networks =
                parse_environment("OES_WEBHOOK_ALLOW_PRIVATE_NETWORKS", value)?;
        }
        if let Some(value) = environment_value(environment, "OES_WEBHOOK_TIMEOUT_SECONDS")? {
            self.webhooks.request_timeout_seconds =
                parse_environment("OES_WEBHOOK_TIMEOUT_SECONDS", value)?;
        }
        if let Some(value) = environment_value(environment, "OES_WEBHOOK_MAXIMUM_ATTEMPTS")? {
            self.webhooks.maximum_attempts =
                parse_environment("OES_WEBHOOK_MAXIMUM_ATTEMPTS", value)?;
        }
        if let Some(value) = environment_value(environment, "OES_WEBHOOK_POLL_INTERVAL_SECONDS")? {
            self.webhooks.poll_interval_seconds =
                parse_environment("OES_WEBHOOK_POLL_INTERVAL_SECONDS", value)?;
        }
        if let Some(value) = environment_value(environment, "OES_LIFECYCLE_INTERVAL_SECONDS")? {
            self.lifecycle.interval_seconds =
                parse_environment("OES_LIFECYCLE_INTERVAL_SECONDS", value)?;
        }
        if let Some(value) = environment_value(environment, "OES_LIFECYCLE_BATCH_SIZE")? {
            self.lifecycle.batch_size = parse_environment("OES_LIFECYCLE_BATCH_SIZE", value)?;
        }
        if let Some(value) = environment_value(environment, "OES_SHARING_SHARES_ENABLED")? {
            self.sharing.shares_enabled = parse_environment("OES_SHARING_SHARES_ENABLED", value)?;
        }
        if let Some(value) = environment_value(environment, "OES_SHARING_EMBEDS_ENABLED")? {
            self.sharing.embeds_enabled = parse_environment("OES_SHARING_EMBEDS_ENABLED", value)?;
        }
        if let Some(value) = environment_value(environment, "OES_SHARING_MAXIMUM_LIFETIME_DAYS")? {
            self.sharing.maximum_lifetime_days =
                parse_environment("OES_SHARING_MAXIMUM_LIFETIME_DAYS", value)?;
        }
        if let Some(value) = environment_value(environment, "OES_SHARING_REQUIRE_EXPIRATION")? {
            self.sharing.require_expiration =
                parse_environment("OES_SHARING_REQUIRE_EXPIRATION", value)?;
        }
        if let Some(value) = environment_value(environment, "OES_SHARING_REQUIRE_PASSWORD")? {
            self.sharing.require_share_password =
                parse_environment("OES_SHARING_REQUIRE_PASSWORD", value)?;
        }
        if let Some(value) = environment_value(environment, "OES_SHARING_MAXIMUM_ACCESS_COUNT")? {
            self.sharing.maximum_access_count =
                parse_environment("OES_SHARING_MAXIMUM_ACCESS_COUNT", value)?;
        }
        if let Some(value) =
            environment_value(environment, "OES_SHARING_PASSWORD_ATTEMPTS_PER_MINUTE")?
        {
            self.sharing.password_attempts_per_minute =
                parse_environment("OES_SHARING_PASSWORD_ATTEMPTS_PER_MINUTE", value)?;
        }
        if let Some(value) = environment_value(environment, "OES_SHARING_TOKEN_PROBES_PER_MINUTE")?
        {
            self.sharing.token_probes_per_minute =
                parse_environment("OES_SHARING_TOKEN_PROBES_PER_MINUTE", value)?;
        }
        if let Some(value) = environment_value(environment, "OES_SHARING_UNLOCK_LIFETIME_HOURS")? {
            self.sharing.unlock_lifetime_hours =
                parse_environment("OES_SHARING_UNLOCK_LIFETIME_HOURS", value)?;
        }
        if let Some(value) = environment_value(environment, "OES_SHARING_PREVIEW_TEXT_LIMIT_BYTES")?
        {
            self.sharing.preview_text_limit_bytes =
                parse_environment("OES_SHARING_PREVIEW_TEXT_LIMIT_BYTES", value)?;
        }
        if let Some(value) = environment_value(environment, "OES_SHARING_SHARE_BASE_URL")? {
            self.sharing.share_base_url = Some(value.to_owned());
        }
        if let Some(value) = environment_value(environment, "OES_SHARING_EMBED_BASE_URL")? {
            self.sharing.embed_base_url = Some(value.to_owned());
        }
        if let Some(value) = environment_value(environment, "OES_CLUSTER_SEEDS")? {
            self.cluster.seeds = value
                .split(',')
                .map(str::trim)
                .filter(|seed| !seed.is_empty())
                .map(str::to_owned)
                .collect();
        }
        if let Some(value) = environment_value(environment, "OES_CLUSTER_JOIN_TOKEN")? {
            self.cluster.join_token = Some(SecretValue::new(value));
        }
        if let Some(value) = environment_value(environment, "OES_CLUSTER_STORAGE_CLASS")? {
            self.cluster.storage_class = value.to_owned();
        }
        if let Some(value) = environment_value(environment, "OES_CLUSTER_FAILURE_DOMAIN")? {
            self.cluster.failure_domain = value.to_owned();
        }
        if let Some(value) = environment_value(environment, "OES_CLUSTER_S3_ENDPOINT")? {
            self.cluster.s3_endpoint = Some(value.to_owned());
        }
        if let Some(value) = environment_value(environment, "OES_CLUSTER_REPLICATION_FACTOR")? {
            self.cluster.replication_factor =
                parse_environment("OES_CLUSTER_REPLICATION_FACTOR", value)?;
        }
        if let Some(value) =
            environment_value(environment, "OES_CLUSTER_CAPACITY_LOW_WATERMARK_PERCENT")?
        {
            self.cluster.capacity_low_watermark_percent =
                parse_environment("OES_CLUSTER_CAPACITY_LOW_WATERMARK_PERCENT", value)?;
        }
        if let Some(value) =
            environment_value(environment, "OES_CLUSTER_CAPACITY_HIGH_WATERMARK_PERCENT")?
        {
            self.cluster.capacity_high_watermark_percent =
                parse_environment("OES_CLUSTER_CAPACITY_HIGH_WATERMARK_PERCENT", value)?;
        }
        if let Some(value) = environment_value(
            environment,
            "OES_CLUSTER_CAPACITY_CRITICAL_WATERMARK_PERCENT",
        )? {
            self.cluster.capacity_critical_watermark_percent =
                parse_environment("OES_CLUSTER_CAPACITY_CRITICAL_WATERMARK_PERCENT", value)?;
        }
        if let Some(value) = environment_value(environment, "OES_CLUSTER_MOVEMENT_CONCURRENCY")? {
            self.cluster.movement_concurrency =
                parse_environment("OES_CLUSTER_MOVEMENT_CONCURRENCY", value)?;
        }
        if let Some(value) =
            environment_value(environment, "OES_CLUSTER_MOVEMENT_BYTES_PER_SECOND")?
        {
            self.cluster.movement_bytes_per_second =
                parse_environment("OES_CLUSTER_MOVEMENT_BYTES_PER_SECOND", value)?;
        }
        if let Some(value) =
            environment_value(environment, "OES_CLUSTER_RECONCILE_INTERVAL_SECONDS")?
        {
            self.cluster.reconcile_interval_seconds =
                parse_environment("OES_CLUSTER_RECONCILE_INTERVAL_SECONDS", value)?;
        }
        if let Some(value) = environment_value(environment, "OES_CLUSTER_TLS_CERTIFICATE")? {
            self.cluster.tls.certificate_path = Some(PathBuf::from(value));
        }
        if let Some(value) = environment_value(environment, "OES_CLUSTER_TLS_PRIVATE_KEY")? {
            self.cluster.tls.private_key_path = Some(PathBuf::from(value));
        }
        if let Some(value) = environment_value(environment, "OES_CLUSTER_TLS_PEER_CA")? {
            self.cluster.tls.peer_ca_path = Some(PathBuf::from(value));
        }
        if let Some(value) = environment_value(environment, "OES_CLUSTER_TLS_CLIENT_CA")? {
            self.cluster.tls.client_ca_path = Some(PathBuf::from(value));
        }
        if let Some(value) = environment_value(environment, "OES_CLUSTER_TLS_SERVER_NAME")? {
            self.cluster.tls.server_name = Some(value.to_owned());
        }
        if let Some(value) = environment_value(environment, "OES_LOG")? {
            self.observability.log_filter = value.to_owned();
        }
        if let Some(value) = environment_value(environment, "OES_LOG_JSON")? {
            self.observability.json = parse_environment("OES_LOG_JSON", value)?;
        }
        Ok(())
    }
}

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

/// Transport security for internal cluster traffic.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterTlsConfig {
    /// PEM certificate chain this node presents to peers.
    pub certificate_path: Option<PathBuf>,
    /// PEM private key for the presented certificate.
    pub private_key_path: Option<PathBuf>,
    /// PEM authority used to verify peer certificates.
    pub peer_ca_path: Option<PathBuf>,
    /// PEM authority used to require and verify peer client certificates.
    ///
    /// Setting this turns on mutual TLS in addition to the node credential that
    /// every internal call already carries.
    pub client_ca_path: Option<PathBuf>,
    /// Server name presented during the handshake, when it differs from the
    /// advertised address.
    pub server_name: Option<String>,
}

/// Node-local cluster settings.
///
/// Cluster-wide policy such as the replication factor, watermarks, and repair
/// limits lives in replicated cluster configuration instead, so that every node
/// agrees on it. Only the values that are genuinely per-process appear here.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterConfig {
    /// Existing members this node contacts to join.
    #[serde(default)]
    pub seeds: Vec<String>,
    /// Single-use token presented when joining.
    pub join_token: Option<SecretValue>,
    /// Storage class this node advertises.
    pub storage_class: String,
    /// Failure-domain labels in `key=value,key=value` form.
    pub failure_domain: String,
    /// Client-facing S3 endpoint this node advertises, when it has one.
    pub s3_endpoint: Option<String>,
    /// Replication factor used when this node initializes a new cluster.
    pub replication_factor: u8,
    /// Low-capacity watermark used when this node initializes a new cluster.
    pub capacity_low_watermark_percent: u32,
    /// High-capacity watermark used when this node initializes a new cluster.
    pub capacity_high_watermark_percent: u32,
    /// Critical-capacity watermark used when this node initializes a new cluster.
    pub capacity_critical_watermark_percent: u32,
    /// Consensus heartbeat interval in milliseconds.
    pub consensus_heartbeat_millis: u64,
    /// Minimum consensus election timeout in milliseconds.
    pub election_timeout_min_millis: u64,
    /// Maximum consensus election timeout in milliseconds.
    pub election_timeout_max_millis: u64,
    /// Log entries appended before a metadata snapshot is built.
    pub snapshot_logs_threshold: u64,
    /// Entries retained after a snapshot, for follower catch-up.
    pub retained_logs: u64,
    /// Replica movements this node runs at once.
    pub movement_concurrency: usize,
    /// Byte-per-second ceiling for one background replica movement.
    pub movement_bytes_per_second: u64,
    /// Seconds between this node's local replica reconciliation passes.
    pub reconcile_interval_seconds: u64,
    /// Transport security for internal traffic.
    #[serde(default)]
    pub tls: ClusterTlsConfig,
}

impl ClusterConfig {
    /// Returns validation problems, given the deployment mode in use.
    fn issues(&self, mode: DeploymentMode) -> Vec<String> {
        let mut issues = Vec::new();
        if !(1..=3).contains(&self.replication_factor) {
            issues.push("cluster.replication_factor must be between 1 and 3".to_owned());
        }
        if self.capacity_low_watermark_percent == 0
            || self.capacity_low_watermark_percent >= self.capacity_high_watermark_percent
            || self.capacity_high_watermark_percent >= self.capacity_critical_watermark_percent
            || self.capacity_critical_watermark_percent > 100
        {
            issues.push(
                "cluster capacity watermarks must satisfy 0 < low < high < critical <= 100"
                    .to_owned(),
            );
        }
        if self.consensus_heartbeat_millis == 0 || self.consensus_heartbeat_millis > 10_000 {
            issues
                .push("cluster.consensus_heartbeat_millis must be between 1 and 10000".to_owned());
        }
        if self.election_timeout_min_millis <= self.consensus_heartbeat_millis * 2 {
            issues.push(
                "cluster.election_timeout_min_millis must exceed twice the consensus heartbeat"
                    .to_owned(),
            );
        }
        if self.election_timeout_max_millis <= self.election_timeout_min_millis {
            issues.push(
                "cluster.election_timeout_max_millis must exceed election_timeout_min_millis"
                    .to_owned(),
            );
        }
        if self.snapshot_logs_threshold == 0 {
            issues.push(
                "cluster.snapshot_logs_threshold must be greater than zero so the consensus log \
                 is compacted"
                    .to_owned(),
            );
        }
        if self.movement_concurrency == 0 || self.movement_concurrency > 256 {
            issues.push("cluster.movement_concurrency must be between 1 and 256".to_owned());
        }
        if self.reconcile_interval_seconds == 0 || self.reconcile_interval_seconds > 86_400 {
            issues
                .push("cluster.reconcile_interval_seconds must be between 1 and 86400".to_owned());
        }
        if self.storage_class.is_empty() || self.storage_class.len() > 32 {
            issues.push("cluster.storage_class must contain between 1 and 32 bytes".to_owned());
        }
        if !self
            .storage_class
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            issues.push(
                "cluster.storage_class may only contain lowercase letters, digits, and hyphens"
                    .to_owned(),
            );
        }
        for entry in self.failure_domain.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            if !entry.contains('=') {
                issues.push(format!(
                    "cluster.failure_domain entry '{entry}' must use key=value form"
                ));
            }
        }
        if self.seeds.len() > 32 {
            issues.push("cluster.seeds must contain at most 32 addresses".to_owned());
        }
        for seed in &self.seeds {
            if seed.trim().is_empty() || seed.len() > 253 {
                issues.push("cluster.seeds entries must be non-empty host:port values".to_owned());
            }
        }
        if self.tls.certificate_path.is_some() != self.tls.private_key_path.is_some() {
            issues.push(
                "cluster.tls.certificate_path and cluster.tls.private_key_path must be configured \
                 together"
                    .to_owned(),
            );
        }
        if self.tls.client_ca_path.is_some() && self.tls.certificate_path.is_none() {
            issues.push(
                "cluster.tls.client_ca_path requires this node to present its own certificate"
                    .to_owned(),
            );
        }
        if mode == DeploymentMode::Control && self.seeds.is_empty() {
            issues.push(
                "a control-plane process needs cluster.seeds so it can reach the cluster"
                    .to_owned(),
            );
        }
        if mode.clustered() && self.join_token.is_some() && self.seeds.is_empty() {
            issues.push(
                "cluster.join_token requires cluster.seeds so the node knows whom to join"
                    .to_owned(),
            );
        }
        issues
    }
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            seeds: Vec::new(),
            join_token: None,
            storage_class: "standard".to_owned(),
            failure_domain: String::new(),
            s3_endpoint: None,
            replication_factor: 3,
            capacity_low_watermark_percent: 80,
            capacity_high_watermark_percent: 90,
            capacity_critical_watermark_percent: 95,
            consensus_heartbeat_millis: 250,
            election_timeout_min_millis: 1_000,
            election_timeout_max_millis: 2_000,
            snapshot_logs_threshold: 8_192,
            retained_logs: 2_048,
            movement_concurrency: 4,
            movement_bytes_per_second: 64 * 1024 * 1024,
            reconcile_interval_seconds: 300,
            tls: ClusterTlsConfig::default(),
        }
    }
}

/// Durable local-storage locations.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    /// Root of all durable OES state.
    pub data_directory: PathBuf,
    /// Optional location for incomplete payload files.
    pub temporary_directory: Option<PathBuf>,
    /// Encrypt newly committed object and multipart payload bytes at rest.
    #[serde(default)]
    pub encryption_enabled: bool,
}

impl StorageConfig {
    /// Returns the explicit temporary directory or `<data_directory>/tmp`.
    #[must_use]
    pub fn effective_temporary_directory(&self) -> PathBuf {
        self.temporary_directory
            .clone()
            .unwrap_or_else(|| self.data_directory.join("tmp"))
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            data_directory: PathBuf::from("./data"),
            temporary_directory: None,
            encryption_enabled: false,
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

/// Deployment policy for external object-access capabilities.
///
/// Every value here narrows what an administrator may create. None of them are
/// enforcement on their own: the capability service re-checks each one, and the
/// public delivery routes re-check revocation and expiry per request. These
/// settings exist so an operator can make a whole deployment stricter than its
/// most careless administrator.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharingConfig {
    /// Whether share links may be created.
    pub shares_enabled: bool,
    /// Whether embed links may be created.
    pub embeds_enabled: bool,
    /// Longest lifetime a new capability may be given, in days.
    ///
    /// Zero means no ceiling, which is a deliberate opt-in rather than the
    /// default: a capability that never expires is one an operator has to keep
    /// track of forever.
    pub maximum_lifetime_days: u32,
    /// Require every new capability to carry an expiry.
    pub require_expiration: bool,
    /// Require every new share link to carry a password.
    pub require_share_password: bool,
    /// Largest access budget a share may be given.
    pub maximum_access_count: u32,
    /// Failed password attempts permitted per share, per client, per window.
    pub password_attempts_per_minute: u32,
    /// Unknown-token lookups permitted per client, per window.
    pub token_probes_per_minute: u32,
    /// How long a share password unlock remains valid, in hours.
    pub unlock_lifetime_hours: u32,
    /// Largest slice of a text or JSON object the console preview will read.
    ///
    /// The console shows the first slice and says so. Nothing about the stored
    /// object changes, and the full bytes remain one download away.
    pub preview_text_limit_bytes: u64,
    /// Public base URL that share links are built from.
    ///
    /// A share link is a page a person opens, so this is the console's public
    /// address. When it is unset the console completes the link against the
    /// origin the administrator is already using, which is right for a
    /// single-origin deployment and wrong behind a rewriting proxy.
    pub share_base_url: Option<String>,
    /// Public base URL that embed links are built from.
    ///
    /// An embed is pasted into somebody else's page and serves object bytes, so
    /// it is published on the S3-compatible storage endpoint rather than on the
    /// console. Keeping the two apart is what lets a deployment expose storage
    /// to the internet while the management plane stays closed.
    ///
    /// When unset this falls back to the advertised S3 endpoint, and then to the
    /// S3 listener address — useful for development, and something a production
    /// deployment behind a proxy or a separate hostname must set explicitly.
    pub embed_base_url: Option<String>,
}

impl Default for SharingConfig {
    fn default() -> Self {
        Self {
            shares_enabled: true,
            embeds_enabled: true,
            maximum_lifetime_days: 365,
            require_expiration: false,
            require_share_password: false,
            maximum_access_count: 10_000,
            password_attempts_per_minute: 10,
            token_probes_per_minute: 60,
            unlock_lifetime_hours: 12,
            preview_text_limit_bytes: 1024 * 1024,
            share_base_url: None,
            embed_base_url: None,
        }
    }
}

impl SharingConfig {
    /// Returns validation problems with the sharing policy.
    fn issues(&self) -> Vec<String> {
        let mut issues = Vec::new();
        if self.maximum_lifetime_days > 3_650 {
            issues.push("sharing.maximum_lifetime_days must be at most 3650".to_owned());
        }
        if self.maximum_access_count == 0 || self.maximum_access_count > 1_000_000 {
            issues.push("sharing.maximum_access_count must be between 1 and 1000000".to_owned());
        }
        if self.password_attempts_per_minute == 0 || self.password_attempts_per_minute > 1_000 {
            issues
                .push("sharing.password_attempts_per_minute must be between 1 and 1000".to_owned());
        }
        if self.token_probes_per_minute == 0 || self.token_probes_per_minute > 100_000 {
            issues.push("sharing.token_probes_per_minute must be between 1 and 100000".to_owned());
        }
        if self.unlock_lifetime_hours == 0 || self.unlock_lifetime_hours > 168 {
            issues.push("sharing.unlock_lifetime_hours must be between 1 and 168".to_owned());
        }
        if self.preview_text_limit_bytes < 1_024
            || self.preview_text_limit_bytes > 64 * 1_024 * 1_024
        {
            issues.push(
                "sharing.preview_text_limit_bytes must be between 1024 and 67108864".to_owned(),
            );
        }
        for (name, value) in [
            ("sharing.share_base_url", &self.share_base_url),
            ("sharing.embed_base_url", &self.embed_base_url),
        ] {
            if let Some(base) = value {
                let trimmed = base.trim();
                if !(trimmed.starts_with("https://") || trimmed.starts_with("http://"))
                    || trimmed.len() > 512
                    || trimmed.contains(char::is_whitespace)
                {
                    issues.push(format!("{name} must be an absolute http or https URL"));
                }
            }
        }
        issues
    }

    /// Returns the share base URL without a trailing slash.
    #[must_use]
    pub fn normalized_share_base_url(&self) -> Option<String> {
        normalize_base_url(self.share_base_url.as_deref())
    }

    /// Returns the embed base URL without a trailing slash, if one was set.
    #[must_use]
    pub fn normalized_embed_base_url(&self) -> Option<String> {
        normalize_base_url(self.embed_base_url.as_deref())
    }
}

fn normalize_base_url(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.trim_end_matches('/').to_owned())
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
            log_filter: "oes=info".to_owned(),
            json: false,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PartialConfig {
    server: Option<PartialServerConfig>,
    storage: Option<PartialStorageConfig>,
    auth: Option<PartialAuthConfig>,
    limits: Option<PartialLimitsConfig>,
    webhooks: Option<PartialWebhookConfig>,
    lifecycle: Option<PartialLifecycleConfig>,
    sharing: Option<PartialSharingConfig>,
    cluster: Option<PartialClusterConfig>,
    observability: Option<PartialObservabilityConfig>,
}

impl PartialConfig {
    fn apply(self, mut target: Config) -> Config {
        if let Some(value) = self.server {
            value.apply(&mut target.server);
        }
        if let Some(value) = self.storage {
            value.apply(&mut target.storage);
        }
        if let Some(value) = self.auth {
            value.apply(&mut target.auth);
        }
        if let Some(value) = self.limits {
            value.apply(&mut target.limits);
        }
        if let Some(value) = self.webhooks {
            value.apply(&mut target.webhooks);
        }
        if let Some(value) = self.lifecycle {
            value.apply(&mut target.lifecycle);
        }
        if let Some(value) = self.sharing {
            value.apply(&mut target.sharing);
        }
        if let Some(value) = self.cluster {
            value.apply(&mut target.cluster);
        }
        if let Some(value) = self.observability {
            value.apply(&mut target.observability);
        }
        target
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PartialServerConfig {
    mode: Option<DeploymentMode>,
    s3_bind: Option<SocketAddr>,
    api_bind: Option<SocketAddr>,
    rpc_bind: Option<SocketAddr>,
    rpc_advertise: Option<String>,
    shutdown_grace_period_seconds: Option<u64>,
}

impl PartialServerConfig {
    fn apply(self, target: &mut ServerConfig) {
        if let Some(value) = self.mode {
            target.mode = value;
        }
        if let Some(value) = self.s3_bind {
            target.s3_bind = value;
        }
        if let Some(value) = self.api_bind {
            target.api_bind = value;
        }
        if let Some(value) = self.rpc_bind {
            target.rpc_bind = value;
        }
        if let Some(value) = self.rpc_advertise {
            target.rpc_advertise = Some(value);
        }
        if let Some(value) = self.shutdown_grace_period_seconds {
            target.shutdown_grace_period_seconds = value;
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PartialClusterConfig {
    seeds: Option<Vec<String>>,
    join_token: Option<SecretValue>,
    storage_class: Option<String>,
    failure_domain: Option<String>,
    s3_endpoint: Option<String>,
    replication_factor: Option<u8>,
    capacity_low_watermark_percent: Option<u32>,
    capacity_high_watermark_percent: Option<u32>,
    capacity_critical_watermark_percent: Option<u32>,
    consensus_heartbeat_millis: Option<u64>,
    election_timeout_min_millis: Option<u64>,
    election_timeout_max_millis: Option<u64>,
    snapshot_logs_threshold: Option<u64>,
    retained_logs: Option<u64>,
    movement_concurrency: Option<usize>,
    movement_bytes_per_second: Option<u64>,
    reconcile_interval_seconds: Option<u64>,
    tls: Option<ClusterTlsConfig>,
}

impl PartialClusterConfig {
    fn apply(self, target: &mut ClusterConfig) {
        if let Some(value) = self.seeds {
            target.seeds = value;
        }
        if let Some(value) = self.join_token {
            target.join_token = Some(value);
        }
        if let Some(value) = self.storage_class {
            target.storage_class = value;
        }
        if let Some(value) = self.failure_domain {
            target.failure_domain = value;
        }
        if let Some(value) = self.s3_endpoint {
            target.s3_endpoint = Some(value);
        }
        if let Some(value) = self.replication_factor {
            target.replication_factor = value;
        }
        if let Some(value) = self.capacity_low_watermark_percent {
            target.capacity_low_watermark_percent = value;
        }
        if let Some(value) = self.capacity_high_watermark_percent {
            target.capacity_high_watermark_percent = value;
        }
        if let Some(value) = self.capacity_critical_watermark_percent {
            target.capacity_critical_watermark_percent = value;
        }
        if let Some(value) = self.consensus_heartbeat_millis {
            target.consensus_heartbeat_millis = value;
        }
        if let Some(value) = self.election_timeout_min_millis {
            target.election_timeout_min_millis = value;
        }
        if let Some(value) = self.election_timeout_max_millis {
            target.election_timeout_max_millis = value;
        }
        if let Some(value) = self.snapshot_logs_threshold {
            target.snapshot_logs_threshold = value;
        }
        if let Some(value) = self.retained_logs {
            target.retained_logs = value;
        }
        if let Some(value) = self.movement_concurrency {
            target.movement_concurrency = value;
        }
        if let Some(value) = self.movement_bytes_per_second {
            target.movement_bytes_per_second = value;
        }
        if let Some(value) = self.reconcile_interval_seconds {
            target.reconcile_interval_seconds = value;
        }
        if let Some(value) = self.tls {
            target.tls = value;
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PartialStorageConfig {
    data_directory: Option<PathBuf>,
    temporary_directory: Option<PathBuf>,
    encryption_enabled: Option<bool>,
}

impl PartialStorageConfig {
    fn apply(self, target: &mut StorageConfig) {
        if let Some(value) = self.data_directory {
            target.data_directory = value;
        }
        if let Some(value) = self.temporary_directory {
            target.temporary_directory = Some(value);
        }
        if let Some(value) = self.encryption_enabled {
            target.encryption_enabled = value;
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PartialAuthConfig {
    root_access_key: Option<String>,
    root_secret_key: Option<SecretValue>,
    credential_master_key: Option<SecretValue>,
    root_s3_enabled: Option<bool>,
    management_system_token: Option<SecretValue>,
    management_storage_token: Option<SecretValue>,
    management_auditor_token: Option<SecretValue>,
    metrics_scrape_token: Option<SecretValue>,
}

impl PartialAuthConfig {
    fn apply(self, target: &mut AuthConfig) {
        if let Some(value) = self.root_access_key {
            target.root_access_key = Some(value);
        }
        if let Some(value) = self.root_secret_key {
            target.root_secret_key = Some(value);
        }
        if let Some(value) = self.credential_master_key {
            target.credential_master_key = Some(value);
        }
        if let Some(value) = self.root_s3_enabled {
            target.root_s3_enabled = value;
        }
        if let Some(value) = self.management_system_token {
            target.management_system_token = Some(value);
        }
        if let Some(value) = self.management_storage_token {
            target.management_storage_token = Some(value);
        }
        if let Some(value) = self.management_auditor_token {
            target.management_auditor_token = Some(value);
        }
        if let Some(value) = self.metrics_scrape_token {
            target.metrics_scrape_token = Some(value);
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PartialLimitsConfig {
    maximum_concurrent_operations: Option<usize>,
    maximum_custom_metadata_entries: Option<usize>,
    maximum_custom_metadata_bytes: Option<usize>,
    maximum_header_bytes: Option<usize>,
}

impl PartialLimitsConfig {
    fn apply(self, target: &mut LimitsConfig) {
        if let Some(value) = self.maximum_concurrent_operations {
            target.maximum_concurrent_operations = value;
        }
        if let Some(value) = self.maximum_custom_metadata_entries {
            target.maximum_custom_metadata_entries = value;
        }
        if let Some(value) = self.maximum_custom_metadata_bytes {
            target.maximum_custom_metadata_bytes = value;
        }
        if let Some(value) = self.maximum_header_bytes {
            target.maximum_header_bytes = value;
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PartialWebhookConfig {
    allow_http: Option<bool>,
    allow_private_networks: Option<bool>,
    request_timeout_seconds: Option<u64>,
    maximum_attempts: Option<u32>,
    poll_interval_seconds: Option<u64>,
}

impl PartialWebhookConfig {
    fn apply(self, target: &mut WebhookConfig) {
        if let Some(value) = self.allow_http {
            target.allow_http = value;
        }
        if let Some(value) = self.allow_private_networks {
            target.allow_private_networks = value;
        }
        if let Some(value) = self.request_timeout_seconds {
            target.request_timeout_seconds = value;
        }
        if let Some(value) = self.maximum_attempts {
            target.maximum_attempts = value;
        }
        if let Some(value) = self.poll_interval_seconds {
            target.poll_interval_seconds = value;
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PartialLifecycleConfig {
    interval_seconds: Option<u64>,
    batch_size: Option<usize>,
}

impl PartialLifecycleConfig {
    fn apply(self, target: &mut LifecycleConfig) {
        if let Some(value) = self.interval_seconds {
            target.interval_seconds = value;
        }
        if let Some(value) = self.batch_size {
            target.batch_size = value;
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PartialSharingConfig {
    shares_enabled: Option<bool>,
    embeds_enabled: Option<bool>,
    maximum_lifetime_days: Option<u32>,
    require_expiration: Option<bool>,
    require_share_password: Option<bool>,
    maximum_access_count: Option<u32>,
    password_attempts_per_minute: Option<u32>,
    token_probes_per_minute: Option<u32>,
    unlock_lifetime_hours: Option<u32>,
    preview_text_limit_bytes: Option<u64>,
    share_base_url: Option<String>,
    embed_base_url: Option<String>,
}

impl PartialSharingConfig {
    fn apply(self, target: &mut SharingConfig) {
        if let Some(value) = self.shares_enabled {
            target.shares_enabled = value;
        }
        if let Some(value) = self.embeds_enabled {
            target.embeds_enabled = value;
        }
        if let Some(value) = self.maximum_lifetime_days {
            target.maximum_lifetime_days = value;
        }
        if let Some(value) = self.require_expiration {
            target.require_expiration = value;
        }
        if let Some(value) = self.require_share_password {
            target.require_share_password = value;
        }
        if let Some(value) = self.maximum_access_count {
            target.maximum_access_count = value;
        }
        if let Some(value) = self.password_attempts_per_minute {
            target.password_attempts_per_minute = value;
        }
        if let Some(value) = self.token_probes_per_minute {
            target.token_probes_per_minute = value;
        }
        if let Some(value) = self.unlock_lifetime_hours {
            target.unlock_lifetime_hours = value;
        }
        if let Some(value) = self.preview_text_limit_bytes {
            target.preview_text_limit_bytes = value;
        }
        if let Some(value) = self.share_base_url {
            target.share_base_url = Some(value);
        }
        if let Some(value) = self.embed_base_url {
            target.embed_base_url = Some(value);
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PartialObservabilityConfig {
    log_filter: Option<String>,
    json: Option<bool>,
}

impl PartialObservabilityConfig {
    fn apply(self, target: &mut ObservabilityConfig) {
        if let Some(value) = self.log_filter {
            target.log_filter = value;
        }
        if let Some(value) = self.json {
            target.json = value;
        }
    }
}

/// Configuration loading and validation failures.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The selected configuration file could not be read.
    #[error("failed to read configuration file '{}': {source}", path.display())]
    ReadFile {
        /// Selected file path.
        path: PathBuf,
        /// Filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// The selected file was not valid OES TOML.
    #[error("failed to parse configuration file '{}': {source}", path.display())]
    ParseFile {
        /// Selected file path.
        path: PathBuf,
        /// TOML decoding error.
        #[source]
        source: toml::de::Error,
    },
    /// An environment variable was not valid Unicode.
    #[error("environment variable {0} is not valid Unicode")]
    NonUnicodeEnvironment(&'static str),
    /// An environment value could not be parsed. Its value is intentionally omitted.
    #[error("environment variable {name} is invalid: {reason}")]
    InvalidEnvironment {
        /// Variable name.
        name: &'static str,
        /// Expected type or parser failure.
        reason: String,
    },
    /// One or more resolved settings were invalid.
    #[error("configuration validation failed: {0}")]
    Validation(String),
}

fn environment_value<'a>(
    environment: &'a HashMap<OsString, OsString>,
    name: &'static str,
) -> Result<Option<&'a str>, ConfigError> {
    let Some(value) = environment.get(&OsString::from(name)) else {
        return Ok(None);
    };
    value
        .to_str()
        .map(Some)
        .ok_or(ConfigError::NonUnicodeEnvironment(name))
}

fn parse_environment<T>(name: &'static str, value: &str) -> Result<T, ConfigError>
where
    T: std::str::FromStr,
    T::Err: Display,
{
    value
        .parse()
        .map_err(|error: T::Err| ConfigError::InvalidEnvironment {
            name,
            reason: error.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    fn credentials() -> [(&'static str, &'static str); 2] {
        [
            ("OES_ROOT_ACCESS_KEY", "test-access"),
            ("OES_ROOT_SECRET_KEY", "test-secret-at-least-sixteen"),
        ]
    }

    fn valid_config() -> Config {
        Config::load_with_environment(None, credentials()).expect("defaults must be valid")
    }

    #[test]
    fn file_and_environment_overlay_defaults_in_order() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("oes.toml");
        fs::write(
            &path,
            r#"
                [server]
                s3_bind = "127.0.0.1:7700"

                [storage]
                data_directory = "/srv/oes"
            "#,
        )
        .expect("write configuration");
        let mut environment = credentials().to_vec();
        environment.push(("OES_API_BIND", "127.0.0.1:7701"));
        environment.push(("OES_LOG", "oes=debug"));
        let config =
            Config::load_with_environment(Some(&path), environment).expect("valid configuration");
        assert_eq!(
            config.server.s3_bind,
            "127.0.0.1:7700".parse().expect("bind")
        );
        assert_eq!(
            config.server.api_bind,
            "127.0.0.1:7701".parse().expect("bind")
        );
        assert_eq!(config.storage.data_directory, PathBuf::from("/srv/oes"));
        assert_eq!(config.observability.log_filter, "oes=debug");
    }

    #[test]
    fn defaults_use_oes_ports_and_require_credentials() {
        let config = Config::default();
        assert_eq!(config.server.s3_bind.port(), 7_600);
        assert_eq!(config.server.api_bind.port(), 7_601);
        assert!(config.validate().is_err());
    }

    #[test]
    fn secrets_are_redacted_from_debug_output() {
        let config = Config::load_with_environment(None, credentials()).expect("configuration");
        let debug = format!("{config:?}");
        assert!(!debug.contains("test-secret-at-least-sixteen"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn metrics_use_a_dedicated_validated_secret() {
        let mut environment = credentials().to_vec();
        environment.push((
            "OES_METRICS_SCRAPE_TOKEN",
            "dedicated-test-metrics-token-at-least-32-bytes",
        ));
        let config =
            Config::load_with_environment(None, environment).expect("metrics token configuration");
        assert_eq!(
            config
                .auth
                .metrics_scrape_token
                .as_ref()
                .expect("configured metrics token")
                .expose(),
            "dedicated-test-metrics-token-at-least-32-bytes"
        );

        let mut duplicate = credentials().to_vec();
        duplicate.extend([
            (
                "OES_MANAGEMENT_SYSTEM_TOKEN",
                "one-shared-token-that-is-at-least-32-bytes",
            ),
            (
                "OES_METRICS_SCRAPE_TOKEN",
                "one-shared-token-that-is-at-least-32-bytes",
            ),
        ]);
        assert!(matches!(
            Config::load_with_environment(None, duplicate),
            Err(ConfigError::Validation(message)) if message.contains("metrics_scrape_token")
        ));
    }

    #[test]
    fn sharing_policy_is_configurable_from_file_and_environment() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("oes.toml");
        fs::write(
            &path,
            r#"
                [sharing]
                require_expiration = true
                maximum_lifetime_days = 30
                share_base_url = "https://oes.example.com/"
                embed_base_url = "https://storage.example.com/"
            "#,
        )
        .expect("write configuration");
        let mut environment = credentials().to_vec();
        environment.push(("OES_SHARING_EMBEDS_ENABLED", "false"));
        environment.push(("OES_SHARING_PASSWORD_ATTEMPTS_PER_MINUTE", "3"));
        let config =
            Config::load_with_environment(Some(&path), environment).expect("valid configuration");

        assert!(config.sharing.shares_enabled);
        assert!(!config.sharing.embeds_enabled);
        assert!(config.sharing.require_expiration);
        assert_eq!(config.sharing.maximum_lifetime_days, 30);
        assert_eq!(config.sharing.password_attempts_per_minute, 3);
        assert_eq!(
            config.sharing.normalized_share_base_url().as_deref(),
            Some("https://oes.example.com")
        );
        // Embeds are published on the storage endpoint, never on the console:
        // a site loading an asset must not have to reach the management plane.
        assert_eq!(
            config.effective_embed_base_url(),
            "https://storage.example.com"
        );
    }

    #[test]
    fn the_embed_address_falls_back_from_config_to_endpoint_to_listener() {
        let mut config = valid_config();
        assert_eq!(
            config.effective_embed_base_url(),
            "http://127.0.0.1:7600",
            "an unspecified bind address must be rendered as something reachable"
        );

        config.cluster.s3_endpoint = Some("storage.internal:7600".to_owned());
        assert_eq!(
            config.effective_embed_base_url(),
            "http://storage.internal:7600"
        );

        config.sharing.embed_base_url = Some("https://cdn.example.com/".to_owned());
        assert_eq!(config.effective_embed_base_url(), "https://cdn.example.com");
    }

    #[test]
    fn sharing_defaults_are_permissive_but_bounded() {
        let config = valid_config();
        assert!(config.sharing.shares_enabled);
        assert!(config.sharing.embeds_enabled);
        assert_eq!(config.sharing.maximum_lifetime_days, 365);
        assert_eq!(config.sharing.preview_text_limit_bytes, 1024 * 1024);
        assert!(config.sharing.normalized_share_base_url().is_none());
        assert!(config.sharing.normalized_embed_base_url().is_none());
    }

    #[test]
    fn unsafe_sharing_policy_values_are_refused_at_load() {
        for (name, value) in [
            ("OES_SHARING_MAXIMUM_ACCESS_COUNT", "0"),
            ("OES_SHARING_PASSWORD_ATTEMPTS_PER_MINUTE", "0"),
            ("OES_SHARING_TOKEN_PROBES_PER_MINUTE", "0"),
            ("OES_SHARING_UNLOCK_LIFETIME_HOURS", "0"),
            ("OES_SHARING_PREVIEW_TEXT_LIMIT_BYTES", "16"),
            ("OES_SHARING_MAXIMUM_LIFETIME_DAYS", "100000"),
            ("OES_SHARING_SHARE_BASE_URL", "javascript:alert(1)"),
            ("OES_SHARING_SHARE_BASE_URL", "oes.example.com"),
            ("OES_SHARING_EMBED_BASE_URL", "javascript:alert(1)"),
            ("OES_SHARING_EMBED_BASE_URL", "storage.example.com"),
        ] {
            let mut environment = credentials().to_vec();
            environment.push((name, value));
            assert!(
                matches!(
                    Config::load_with_environment(None, environment),
                    Err(ConfigError::Validation(_))
                ),
                "accepted unsafe sharing value {name}={value}"
            );
        }
    }

    #[test]
    fn unknown_file_fields_and_invalid_environment_are_rejected() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("oes.toml");
        fs::write(&path, "[server]\nsecret_backdoor = true\n").expect("write configuration");
        assert!(matches!(
            Config::load_with_environment(Some(&path), credentials()),
            Err(ConfigError::ParseFile { .. })
        ));
        let mut environment = credentials().to_vec();
        environment.push(("OES_S3_BIND", "not-an-address"));
        let error =
            Config::load_with_environment(None, environment).expect_err("invalid environment");
        assert!(error.to_string().contains("OES_S3_BIND"));
        assert!(!error.to_string().contains("test-secret"));
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
    fn object_encryption_requires_the_explicit_master_key() {
        let mut without_key = credentials().to_vec();
        without_key.push(("OES_STORAGE_ENCRYPTION_ENABLED", "true"));
        assert!(matches!(
            Config::load_with_environment(None, without_key),
            Err(ConfigError::Validation(message)) if message.contains("credential_master_key")
        ));

        let mut configured = credentials().to_vec();
        configured.push(("OES_STORAGE_ENCRYPTION_ENABLED", "true"));
        configured.push((
            "OES_CREDENTIAL_MASTER_KEY",
            "stable-test-master-key-at-least-thirty-two-bytes",
        ));
        let config = Config::load_with_environment(None, configured).expect("encrypted config");
        assert!(config.storage.encryption_enabled);
    }

    #[test]
    fn default_listeners_use_the_documented_oes_ports() {
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
                "OES must not default to another product's port"
            );
            assert_ne!(
                port, 9_001,
                "OES must not default to another product's port"
            );
        }
        assert_eq!(server.mode, DeploymentMode::Standalone);
        assert_eq!(
            server.effective_rpc_advertise(),
            server.rpc_bind.to_string()
        );
    }

    #[test]
    fn listeners_must_be_distinct_and_avoid_the_reserved_console_port() {
        let mut config = valid_config();
        config.server.rpc_bind = config.server.api_bind;
        assert!(config.validate().is_err());

        let mut config = valid_config();
        config.server.rpc_bind = "0.0.0.0:7602".parse().expect("address");
        let error = config
            .validate()
            .expect_err("the reserved console port must be refused");
        assert!(error.to_string().contains("7602"));
    }

    #[test]
    fn cluster_settings_are_validated_strictly() {
        let mut config = valid_config();
        config.cluster.replication_factor = 4;
        assert!(config.validate().is_err());

        let mut config = valid_config();
        config.cluster.storage_class = "NVMe".to_owned();
        assert!(config.validate().is_err());

        let mut config = valid_config();
        config.cluster.failure_domain = "rack".to_owned();
        assert!(config.validate().is_err());

        let mut config = valid_config();
        config.cluster.election_timeout_min_millis = 100;
        assert!(config.validate().is_err());

        let mut config = valid_config();
        config.server.mode = DeploymentMode::Control;
        let error = config
            .validate()
            .expect_err("a control process without seeds cannot reach the cluster");
        assert!(error.to_string().contains("cluster.seeds"));

        let mut config = valid_config();
        config.cluster.tls.certificate_path = Some(PathBuf::from("/tmp/cert.pem"));
        assert!(config.validate().is_err());
    }

    #[test]
    fn cluster_environment_overrides_are_applied() {
        let config = Config::load_with_environment(
            None,
            [
                ("OES_ROOT_ACCESS_KEY", "root-access"),
                ("OES_ROOT_SECRET_KEY", "root-secret-at-least-sixteen"),
                ("OES_MODE", "cluster"),
                ("OES_RPC_BIND", "0.0.0.0:17603"),
                ("OES_RPC_ADVERTISE", "10.0.1.12:17603"),
                ("OES_CLUSTER_SEEDS", "storage-1:7603, storage-2:7603"),
                ("OES_CLUSTER_JOIN_TOKEN", "oesjoin.token"),
                ("OES_CLUSTER_STORAGE_CLASS", "nvme"),
                ("OES_CLUSTER_FAILURE_DOMAIN", "rack=r1,zone=dc1"),
                ("OES_CLUSTER_REPLICATION_FACTOR", "2"),
                ("OES_CLUSTER_CAPACITY_LOW_WATERMARK_PERCENT", "70"),
                ("OES_CLUSTER_CAPACITY_HIGH_WATERMARK_PERCENT", "80"),
                ("OES_CLUSTER_CAPACITY_CRITICAL_WATERMARK_PERCENT", "90"),
            ],
        )
        .expect("configuration must load");
        assert_eq!(config.server.mode, DeploymentMode::Cluster);
        assert_eq!(config.server.rpc_bind.port(), 17_603);
        assert_eq!(
            config.server.effective_rpc_advertise(),
            "10.0.1.12:17603",
            "an advertise address must not be assumed equal to the bind address"
        );
        assert_eq!(config.cluster.seeds.len(), 2);
        assert_eq!(config.cluster.storage_class, "nvme");
        assert_eq!(config.cluster.replication_factor, 2);
        assert_eq!(config.cluster.capacity_low_watermark_percent, 70);
        assert_eq!(config.cluster.capacity_high_watermark_percent, 80);
        assert_eq!(config.cluster.capacity_critical_watermark_percent, 90);
        assert!(format!("{:?}", config.cluster.join_token).contains("redacted"));
    }
}
