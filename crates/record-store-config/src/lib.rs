//! Configuration loading, environment overrides, secret redaction, and validation.

use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    fmt::{self, Debug, Formatter},
    fs,
    path::Path,
};

use serde::Deserialize;

use crate::partial::PartialConfig;

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

mod cluster;
mod deployment;
mod environment;
mod error;
mod partial;
mod sections;
mod sharing;
mod validate;

#[cfg(test)]
mod test_support;

pub use cluster::{ClusterConfig, ClusterTlsConfig};
pub use deployment::DeploymentMode;
pub use error::ConfigError;
pub use sections::{
    AuthConfig, LifecycleConfig, LimitsConfig, ObservabilityConfig, ServerConfig, StorageConfig,
    WebhookConfig,
};
pub use sharing::SharingConfig;
/// Fully resolved and validated Record Store configuration.
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
    /// Loads defaults, overlays an optional TOML file, overlays `RECORD_STORE_*`
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

    /// Returns configured root credentials after validation.
    pub fn root_credentials(&self) -> Result<(&str, &SecretValue), ConfigError> {
        self.auth
            .root_access_key
            .as_deref()
            .zip(self.auth.root_secret_key.as_ref())
            .ok_or_else(|| ConfigError::Validation("root credentials are required".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::credentials;

    #[test]
    fn secrets_are_redacted_from_debug_output() {
        let config = Config::load_with_environment(None, credentials()).expect("configuration");
        let debug = format!("{config:?}");
        assert!(!debug.contains("test-secret-at-least-sixteen"));
        assert!(debug.contains("<redacted>"));
    }
}
