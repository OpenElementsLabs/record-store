//! Configuration loading, environment overrides, secret redaction, and validation.

use crate::*;

impl Config {
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
        // Declared devices are independent placement targets, so two of them
        // sharing a name or a path would make the cluster believe it has more
        // failure independence than it does.
        let mut device_names = std::collections::BTreeSet::new();
        let mut device_paths = std::collections::BTreeSet::new();
        for device in &self.storage.devices {
            if device.name.is_empty() {
                issues.push("storage.devices[].name must not be empty".to_owned());
            } else if !device
                .name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            {
                issues.push(format!(
                    "storage.devices[].name '{}' may only contain letters, digits, hyphens, and underscores",
                    device.name
                ));
            } else if !device_names.insert(device.name.as_str()) {
                issues.push(format!(
                    "storage.devices[].name '{}' is declared more than once",
                    device.name
                ));
            }
            if device.path.as_os_str().is_empty() {
                issues.push("storage.devices[].path must not be empty".to_owned());
            } else if !device_paths.insert(device.path.as_path()) {
                issues.push(format!(
                    "storage.devices[].path '{}' is declared more than once",
                    device.path.display()
                ));
            } else if device.path == self.storage.data_directory {
                issues.push(format!(
                    "storage.devices[].path '{}' is already the node's data_directory",
                    device.path.display()
                ));
            }
            if device
                .weight
                .is_some_and(|weight| weight == 0 || weight > 10_000)
            {
                issues.push(format!(
                    "storage.devices[] '{}' weight must be between 1 and 10000",
                    device.name
                ));
            }
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
                "root credentials are required; set RECORD_STORE_ROOT_ACCESS_KEY and RECORD_STORE_ROOT_SECRET_KEY"
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
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::Config;
    use crate::test_support::*;

    /// Two devices sharing a name or a path would make the cluster believe it
    /// has more failure independence than it does, so both are refused.
    #[test]
    fn declared_devices_must_be_distinct_and_separate_from_the_data_directory() {
        use crate::sections::StorageDeviceConfig;

        fn device(name: &str, path: &str) -> StorageDeviceConfig {
            StorageDeviceConfig {
                name: name.to_owned(),
                path: PathBuf::from(path),
                storage_class: None,
                weight: None,
                movement_concurrency: None,
            }
        }

        let base = valid_config();

        let mut config = base.clone();
        config.storage.devices = vec![device("nvme0", "/mnt/nvme0"), device("nvme1", "/mnt/nvme1")];
        config.validate().expect("two distinct devices are valid");

        let mut duplicate_name = base.clone();
        duplicate_name.storage.devices = vec![device("nvme0", "/mnt/a"), device("nvme0", "/mnt/b")];
        let error = duplicate_name
            .validate()
            .expect_err("a repeated device name is refused");
        assert!(
            error.to_string().contains("declared more than once"),
            "{error}"
        );

        let mut duplicate_path = base.clone();
        duplicate_path.storage.devices = vec![device("nvme0", "/mnt/a"), device("nvme1", "/mnt/a")];
        assert!(
            duplicate_path.validate().is_err(),
            "two devices on one path are not two devices"
        );

        let mut collides = base.clone();
        collides.storage.devices = vec![device("root", "./data")];
        let error = collides
            .validate()
            .expect_err("the data directory is already a device");
        assert!(error.to_string().contains("data_directory"), "{error}");

        let mut unnamed = base.clone();
        unnamed.storage.devices = vec![device("", "/mnt/a")];
        assert!(unnamed.validate().is_err(), "a device needs a name");

        let mut odd_name = base.clone();
        odd_name.storage.devices = vec![device("nvme 0", "/mnt/a")];
        assert!(
            odd_name.validate().is_err(),
            "a device name is an identifier, not free text"
        );

        let mut weightless = base;
        weightless.storage.devices = vec![StorageDeviceConfig {
            weight: Some(0),
            ..device("nvme0", "/mnt/a")
        }];
        assert!(
            weightless.validate().is_err(),
            "a zero weight places nothing"
        );
    }

    /// A node with no declared devices behaves exactly as it did before they
    /// existed, which is what keeps standalone and existing clusters unchanged.
    #[test]
    fn declaring_no_devices_is_valid_and_changes_nothing() {
        let config = valid_config();
        assert!(config.storage.devices.is_empty());
        config.validate().expect("valid");
    }

    #[test]
    fn metrics_use_a_dedicated_validated_secret() {
        let mut environment = credentials().to_vec();
        environment.push((
            "RECORD_STORE_METRICS_SCRAPE_TOKEN",
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
                "RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN",
                "one-shared-token-that-is-at-least-32-bytes",
            ),
            (
                "RECORD_STORE_METRICS_SCRAPE_TOKEN",
                "one-shared-token-that-is-at-least-32-bytes",
            ),
        ]);
        assert!(matches!(
            Config::load_with_environment(None, duplicate),
            Err(ConfigError::Validation(message)) if message.contains("metrics_scrape_token")
        ));
    }

    #[test]
    fn unsafe_sharing_policy_values_are_refused_at_load() {
        for (name, value) in [
            ("RECORD_STORE_SHARING_MAXIMUM_ACCESS_COUNT", "0"),
            ("RECORD_STORE_SHARING_PASSWORD_ATTEMPTS_PER_MINUTE", "0"),
            ("RECORD_STORE_SHARING_TOKEN_PROBES_PER_MINUTE", "0"),
            ("RECORD_STORE_SHARING_UNLOCK_LIFETIME_HOURS", "0"),
            ("RECORD_STORE_SHARING_PREVIEW_TEXT_LIMIT_BYTES", "16"),
            ("RECORD_STORE_SHARING_MAXIMUM_LIFETIME_DAYS", "100000"),
            ("RECORD_STORE_SHARING_SHARE_BASE_URL", "javascript:alert(1)"),
            (
                "RECORD_STORE_SHARING_SHARE_BASE_URL",
                "record-store.example.com",
            ),
            ("RECORD_STORE_SHARING_EMBED_BASE_URL", "javascript:alert(1)"),
            ("RECORD_STORE_SHARING_EMBED_BASE_URL", "storage.example.com"),
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
    fn object_encryption_requires_the_explicit_master_key() {
        let mut without_key = credentials().to_vec();
        without_key.push(("RECORD_STORE_STORAGE_ENCRYPTION_ENABLED", "true"));
        assert!(matches!(
            Config::load_with_environment(None, without_key),
            Err(ConfigError::Validation(message)) if message.contains("credential_master_key")
        ));

        let mut configured = credentials().to_vec();
        configured.push(("RECORD_STORE_STORAGE_ENCRYPTION_ENABLED", "true"));
        configured.push((
            "RECORD_STORE_CREDENTIAL_MASTER_KEY",
            "stable-test-master-key-at-least-thirty-two-bytes",
        ));
        let config = Config::load_with_environment(None, configured).expect("encrypted config");
        assert!(config.storage.encryption_enabled);
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
}
