//! Configuration loading, environment overrides, and validation.

use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    fmt::Display,
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

const ENV_PREFIX: &str = "OES_";

/// Fully resolved and validated OES configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// HTTP server settings.
    pub server: ServerConfig,
    /// Local storage settings.
    pub storage: StorageConfig,
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
    ///
    /// This is useful for deterministic tests and process supervisors that
    /// already maintain a sanitized environment map.
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
        if self.server.port == 0 {
            issues.push("server.port must be greater than zero".to_owned());
        }
        if self.server.max_request_size_bytes == 0 {
            issues.push("server.max_request_size_bytes must be greater than zero".to_owned());
        }
        if self.server.max_request_size_bytes > usize::MAX as u64 {
            issues.push("server.max_request_size_bytes does not fit this platform".to_owned());
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
        if self.observability.log_filter.trim().is_empty() {
            issues.push("observability.log_filter must not be empty".to_owned());
        }

        if issues.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Validation(issues.join("; ")))
        }
    }

    /// Returns the configured listening socket.
    #[must_use]
    pub const fn listen_address(&self) -> SocketAddr {
        SocketAddr::new(self.server.bind_address, self.server.port)
    }

    fn apply_environment(
        &mut self,
        environment: &HashMap<OsString, OsString>,
    ) -> Result<(), ConfigError> {
        if let Some(value) = environment_value(environment, "OES_SERVER_BIND_ADDRESS")? {
            self.server.bind_address = parse_environment("OES_SERVER_BIND_ADDRESS", value)?;
        }
        if let Some(value) = environment_value(environment, "OES_SERVER_PORT")? {
            self.server.port = parse_environment("OES_SERVER_PORT", value)?;
        }
        if let Some(value) = environment_value(environment, "OES_SERVER_MAX_REQUEST_SIZE_BYTES")? {
            self.server.max_request_size_bytes =
                parse_environment("OES_SERVER_MAX_REQUEST_SIZE_BYTES", value)?;
        }
        if let Some(value) =
            environment_value(environment, "OES_SERVER_SHUTDOWN_GRACE_PERIOD_SECONDS")?
        {
            self.server.shutdown_grace_period_seconds =
                parse_environment("OES_SERVER_SHUTDOWN_GRACE_PERIOD_SECONDS", value)?;
        }
        if let Some(value) = environment_value(environment, "OES_STORAGE_DATA_DIRECTORY")? {
            self.storage.data_directory = PathBuf::from(value);
        }
        if let Some(value) = environment_value(environment, "OES_STORAGE_TEMPORARY_DIRECTORY")? {
            self.storage.temporary_directory = Some(PathBuf::from(value));
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

/// HTTP listener and request-limit settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Address on which the HTTP server listens.
    pub bind_address: IpAddr,
    /// TCP port on which the HTTP server listens.
    pub port: u16,
    /// Maximum request body size accepted by HTTP routes.
    pub max_request_size_bytes: u64,
    /// Maximum graceful-shutdown drain time.
    pub shutdown_grace_period_seconds: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_address: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 9_000,
            max_request_size_bytes: 64 * 1024 * 1024,
            shutdown_grace_period_seconds: 30,
        }
    }
}

/// Durable local-storage locations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// Structured logging settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservabilityConfig {
    /// `tracing-subscriber` filter expression.
    pub log_filter: String,
    /// Emit newline-delimited JSON when true; human-readable logs otherwise.
    pub json: bool,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            log_filter: "oes=info,tower_http=info".to_owned(),
            json: false,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PartialConfig {
    server: Option<PartialServerConfig>,
    storage: Option<PartialStorageConfig>,
    observability: Option<PartialObservabilityConfig>,
}

impl PartialConfig {
    fn apply(self, mut target: Config) -> Config {
        if let Some(server) = self.server {
            server.apply(&mut target.server);
        }
        if let Some(storage) = self.storage {
            storage.apply(&mut target.storage);
        }
        if let Some(observability) = self.observability {
            observability.apply(&mut target.observability);
        }
        target
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PartialServerConfig {
    bind_address: Option<IpAddr>,
    port: Option<u16>,
    max_request_size_bytes: Option<u64>,
    shutdown_grace_period_seconds: Option<u64>,
}

impl PartialServerConfig {
    fn apply(self, target: &mut ServerConfig) {
        if let Some(value) = self.bind_address {
            target.bind_address = value;
        }
        if let Some(value) = self.port {
            target.port = value;
        }
        if let Some(value) = self.max_request_size_bytes {
            target.max_request_size_bytes = value;
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

/// Returns true when a name belongs to the OES process environment namespace.
#[must_use]
pub fn is_oes_environment_variable(name: &str) -> bool {
    name.starts_with(ENV_PREFIX)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn file_and_environment_overlay_defaults_in_order() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("oes.toml");
        fs::write(
            &path,
            r#"
                [server]
                bind_address = "127.0.0.1"
                port = 9100

                [storage]
                data_directory = "/srv/oes"
            "#,
        )
        .expect("write configuration");

        let config = Config::load_with_environment(
            Some(&path),
            [
                ("OES_SERVER_PORT", "9200"),
                ("OES_LOG", "oes=debug"),
                ("UNRELATED", "ignored"),
            ],
        )
        .expect("valid configuration");

        assert_eq!(config.server.port, 9_200);
        assert_eq!(
            config.server.bind_address,
            "127.0.0.1".parse::<IpAddr>().expect("IP")
        );
        assert_eq!(config.storage.data_directory, PathBuf::from("/srv/oes"));
        assert_eq!(config.observability.log_filter, "oes=debug");
        assert_eq!(config.server.max_request_size_bytes, 64 * 1024 * 1024);
    }

    #[test]
    fn unknown_file_fields_are_rejected() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("oes.toml");
        fs::write(&path, "[server]\nsecret_backdoor = true\n").expect("write configuration");
        assert!(matches!(
            Config::load_with_environment(Some(&path), std::iter::empty::<(&str, &str)>()),
            Err(ConfigError::ParseFile { .. })
        ));
    }

    #[test]
    fn invalid_values_have_human_readable_errors() {
        let mut config = Config::default();
        config.server.port = 0;
        config.server.max_request_size_bytes = 0;
        let error = config.validate().expect_err("invalid configuration");
        let message = error.to_string();
        assert!(message.contains("server.port"));
        assert!(message.contains("server.max_request_size_bytes"));

        let error = Config::load_with_environment(None, [("OES_SERVER_PORT", "not-a-port")])
            .expect_err("invalid environment");
        assert!(error.to_string().contains("OES_SERVER_PORT"));
        assert!(!error.to_string().contains("not-a-port"));
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
