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

use serde::Deserialize;
use thiserror::Error;

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
    /// Root and credential-encryption settings.
    pub auth: AuthConfig,
    /// Request and concurrency limits.
    pub limits: LimitsConfig,
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

    /// Validates cross-field and security-sensitive constraints.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut issues = Vec::new();
        if self.server.s3_bind.port() == 0 {
            issues.push("server.s3_bind port must be greater than zero".to_owned());
        }
        if self.server.api_bind.port() == 0 {
            issues.push("server.api_bind port must be greater than zero".to_owned());
        }
        if self.server.s3_bind == self.server.api_bind {
            issues.push("server.s3_bind and server.api_bind must be different".to_owned());
        }
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
        if let Some(value) = environment_value(environment, "OES_S3_BIND")? {
            self.server.s3_bind = parse_environment("OES_S3_BIND", value)?;
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
        if let Some(value) = environment_value(environment, "OES_ROOT_ACCESS_KEY")? {
            self.auth.root_access_key = Some(value.to_owned());
        }
        if let Some(value) = environment_value(environment, "OES_ROOT_SECRET_KEY")? {
            self.auth.root_secret_key = Some(SecretValue::new(value));
        }
        if let Some(value) = environment_value(environment, "OES_CREDENTIAL_MASTER_KEY")? {
            self.auth.credential_master_key = Some(SecretValue::new(value));
        }
        if let Some(value) = environment_value(environment, "OES_MAX_CONCURRENT_OPERATIONS")? {
            self.limits.maximum_concurrent_operations =
                parse_environment("OES_MAX_CONCURRENT_OPERATIONS", value)?;
        }
        if let Some(value) = environment_value(environment, "OES_MAX_HEADER_BYTES")? {
            self.limits.maximum_header_bytes = parse_environment("OES_MAX_HEADER_BYTES", value)?;
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

/// Listener and shutdown settings.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// S3-compatible API listener.
    pub s3_bind: SocketAddr,
    /// Native management API listener.
    pub api_bind: SocketAddr,
    /// Maximum graceful-shutdown drain time.
    pub shutdown_grace_period_seconds: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            s3_bind: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 7_600)),
            api_bind: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 7_601)),
            shutdown_grace_period_seconds: 30,
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
        }
    }
}

/// Credential bootstrap and encryption settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Root S3 access key identifier.
    pub root_access_key: Option<String>,
    /// Root S3 secret key.
    pub root_secret_key: Option<SecretValue>,
    /// Optional dedicated key for service-account secret encryption.
    pub credential_master_key: Option<SecretValue>,
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
        if let Some(value) = self.observability {
            value.apply(&mut target.observability);
        }
        target
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PartialServerConfig {
    s3_bind: Option<SocketAddr>,
    api_bind: Option<SocketAddr>,
    shutdown_grace_period_seconds: Option<u64>,
}

impl PartialServerConfig {
    fn apply(self, target: &mut ServerConfig) {
        if let Some(value) = self.s3_bind {
            target.s3_bind = value;
        }
        if let Some(value) = self.api_bind {
            target.api_bind = value;
        }
        if let Some(value) = self.shutdown_grace_period_seconds {
            target.shutdown_grace_period_seconds = value;
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PartialStorageConfig {
    data_directory: Option<PathBuf>,
    temporary_directory: Option<PathBuf>,
}

impl PartialStorageConfig {
    fn apply(self, target: &mut StorageConfig) {
        if let Some(value) = self.data_directory {
            target.data_directory = value;
        }
        if let Some(value) = self.temporary_directory {
            target.temporary_directory = Some(value);
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PartialAuthConfig {
    root_access_key: Option<String>,
    root_secret_key: Option<SecretValue>,
    credential_master_key: Option<SecretValue>,
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
}
