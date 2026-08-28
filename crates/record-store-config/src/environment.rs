//! Configuration loading, environment overrides, secret redaction, and validation.

use std::{collections::HashMap, ffi::OsString, path::PathBuf};

use crate::error::{environment_value, parse_environment};
use crate::*;

impl Config {
    pub(crate) fn apply_environment(
        &mut self,
        environment: &HashMap<OsString, OsString>,
    ) -> Result<(), ConfigError> {
        if let Some(value) = environment_value(environment, "RECORD_STORE_MODE")? {
            self.server.mode = value.parse()?;
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_S3_BIND")? {
            self.server.s3_bind = parse_environment("RECORD_STORE_S3_BIND", value)?;
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_RPC_BIND")? {
            self.server.rpc_bind = parse_environment("RECORD_STORE_RPC_BIND", value)?;
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_RPC_ADVERTISE")? {
            self.server.rpc_advertise = Some(value.to_owned());
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_API_BIND")? {
            self.server.api_bind = parse_environment("RECORD_STORE_API_BIND", value)?;
        }
        if let Some(value) =
            environment_value(environment, "RECORD_STORE_SHUTDOWN_TIMEOUT_SECONDS")?
        {
            self.server.shutdown_grace_period_seconds =
                parse_environment("RECORD_STORE_SHUTDOWN_TIMEOUT_SECONDS", value)?;
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_STORAGE_DATA_DIRECTORY")?
        {
            self.storage.data_directory = PathBuf::from(value);
        }
        if let Some(value) =
            environment_value(environment, "RECORD_STORE_STORAGE_TEMPORARY_DIRECTORY")?
        {
            self.storage.temporary_directory = Some(PathBuf::from(value));
        }
        if let Some(value) =
            environment_value(environment, "RECORD_STORE_STORAGE_ENCRYPTION_ENABLED")?
        {
            self.storage.encryption_enabled =
                parse_environment("RECORD_STORE_STORAGE_ENCRYPTION_ENABLED", value)?;
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_ROOT_ACCESS_KEY")? {
            self.auth.root_access_key = Some(value.to_owned());
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_ROOT_SECRET_KEY")? {
            self.auth.root_secret_key = Some(SecretValue::new(value));
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_CREDENTIAL_MASTER_KEY")? {
            self.auth.credential_master_key = Some(SecretValue::new(value));
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_ROOT_S3_ENABLED")? {
            self.auth.root_s3_enabled = parse_environment("RECORD_STORE_ROOT_S3_ENABLED", value)?;
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN")?
        {
            self.auth.management_system_token = Some(SecretValue::new(value));
        }
        if let Some(value) =
            environment_value(environment, "RECORD_STORE_MANAGEMENT_STORAGE_TOKEN")?
        {
            self.auth.management_storage_token = Some(SecretValue::new(value));
        }
        if let Some(value) =
            environment_value(environment, "RECORD_STORE_MANAGEMENT_AUDITOR_TOKEN")?
        {
            self.auth.management_auditor_token = Some(SecretValue::new(value));
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_METRICS_SCRAPE_TOKEN")? {
            self.auth.metrics_scrape_token = Some(SecretValue::new(value));
        }
        if let Some(value) =
            environment_value(environment, "RECORD_STORE_MAX_CONCURRENT_OPERATIONS")?
        {
            self.limits.maximum_concurrent_operations =
                parse_environment("RECORD_STORE_MAX_CONCURRENT_OPERATIONS", value)?;
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_MAX_HEADER_BYTES")? {
            self.limits.maximum_header_bytes =
                parse_environment("RECORD_STORE_MAX_HEADER_BYTES", value)?;
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_WEBHOOK_ALLOW_HTTP")? {
            self.webhooks.allow_http = parse_environment("RECORD_STORE_WEBHOOK_ALLOW_HTTP", value)?;
        }
        if let Some(value) =
            environment_value(environment, "RECORD_STORE_WEBHOOK_ALLOW_PRIVATE_NETWORKS")?
        {
            self.webhooks.allow_private_networks =
                parse_environment("RECORD_STORE_WEBHOOK_ALLOW_PRIVATE_NETWORKS", value)?;
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_WEBHOOK_TIMEOUT_SECONDS")?
        {
            self.webhooks.request_timeout_seconds =
                parse_environment("RECORD_STORE_WEBHOOK_TIMEOUT_SECONDS", value)?;
        }
        if let Some(value) =
            environment_value(environment, "RECORD_STORE_WEBHOOK_MAXIMUM_ATTEMPTS")?
        {
            self.webhooks.maximum_attempts =
                parse_environment("RECORD_STORE_WEBHOOK_MAXIMUM_ATTEMPTS", value)?;
        }
        if let Some(value) =
            environment_value(environment, "RECORD_STORE_WEBHOOK_POLL_INTERVAL_SECONDS")?
        {
            self.webhooks.poll_interval_seconds =
                parse_environment("RECORD_STORE_WEBHOOK_POLL_INTERVAL_SECONDS", value)?;
        }
        if let Some(value) =
            environment_value(environment, "RECORD_STORE_LIFECYCLE_INTERVAL_SECONDS")?
        {
            self.lifecycle.interval_seconds =
                parse_environment("RECORD_STORE_LIFECYCLE_INTERVAL_SECONDS", value)?;
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_LIFECYCLE_BATCH_SIZE")? {
            self.lifecycle.batch_size =
                parse_environment("RECORD_STORE_LIFECYCLE_BATCH_SIZE", value)?;
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_SHARING_SHARES_ENABLED")?
        {
            self.sharing.shares_enabled =
                parse_environment("RECORD_STORE_SHARING_SHARES_ENABLED", value)?;
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_SHARING_EMBEDS_ENABLED")?
        {
            self.sharing.embeds_enabled =
                parse_environment("RECORD_STORE_SHARING_EMBEDS_ENABLED", value)?;
        }
        if let Some(value) =
            environment_value(environment, "RECORD_STORE_SHARING_MAXIMUM_LIFETIME_DAYS")?
        {
            self.sharing.maximum_lifetime_days =
                parse_environment("RECORD_STORE_SHARING_MAXIMUM_LIFETIME_DAYS", value)?;
        }
        if let Some(value) =
            environment_value(environment, "RECORD_STORE_SHARING_REQUIRE_EXPIRATION")?
        {
            self.sharing.require_expiration =
                parse_environment("RECORD_STORE_SHARING_REQUIRE_EXPIRATION", value)?;
        }
        if let Some(value) =
            environment_value(environment, "RECORD_STORE_SHARING_REQUIRE_PASSWORD")?
        {
            self.sharing.require_share_password =
                parse_environment("RECORD_STORE_SHARING_REQUIRE_PASSWORD", value)?;
        }
        if let Some(value) =
            environment_value(environment, "RECORD_STORE_SHARING_MAXIMUM_ACCESS_COUNT")?
        {
            self.sharing.maximum_access_count =
                parse_environment("RECORD_STORE_SHARING_MAXIMUM_ACCESS_COUNT", value)?;
        }
        if let Some(value) = environment_value(
            environment,
            "RECORD_STORE_SHARING_PASSWORD_ATTEMPTS_PER_MINUTE",
        )? {
            self.sharing.password_attempts_per_minute =
                parse_environment("RECORD_STORE_SHARING_PASSWORD_ATTEMPTS_PER_MINUTE", value)?;
        }
        if let Some(value) =
            environment_value(environment, "RECORD_STORE_SHARING_TOKEN_PROBES_PER_MINUTE")?
        {
            self.sharing.token_probes_per_minute =
                parse_environment("RECORD_STORE_SHARING_TOKEN_PROBES_PER_MINUTE", value)?;
        }
        if let Some(value) =
            environment_value(environment, "RECORD_STORE_SHARING_UNLOCK_LIFETIME_HOURS")?
        {
            self.sharing.unlock_lifetime_hours =
                parse_environment("RECORD_STORE_SHARING_UNLOCK_LIFETIME_HOURS", value)?;
        }
        if let Some(value) =
            environment_value(environment, "RECORD_STORE_SHARING_PREVIEW_TEXT_LIMIT_BYTES")?
        {
            self.sharing.preview_text_limit_bytes =
                parse_environment("RECORD_STORE_SHARING_PREVIEW_TEXT_LIMIT_BYTES", value)?;
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_SHARING_SHARE_BASE_URL")?
        {
            self.sharing.share_base_url = Some(value.to_owned());
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_SHARING_EMBED_BASE_URL")?
        {
            self.sharing.embed_base_url = Some(value.to_owned());
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_CLUSTER_SEEDS")? {
            self.cluster.seeds = value
                .split(',')
                .map(str::trim)
                .filter(|seed| !seed.is_empty())
                .map(str::to_owned)
                .collect();
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_CLUSTER_JOIN_TOKEN")? {
            self.cluster.join_token = Some(SecretValue::new(value));
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_CLUSTER_STORAGE_CLASS")? {
            self.cluster.storage_class = value.to_owned();
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_CLUSTER_FAILURE_DOMAIN")?
        {
            self.cluster.failure_domain = value.to_owned();
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_CLUSTER_S3_ENDPOINT")? {
            self.cluster.s3_endpoint = Some(value.to_owned());
        }
        if let Some(value) =
            environment_value(environment, "RECORD_STORE_CLUSTER_REPLICATION_FACTOR")?
        {
            self.cluster.replication_factor =
                parse_environment("RECORD_STORE_CLUSTER_REPLICATION_FACTOR", value)?;
        }
        if let Some(value) = environment_value(
            environment,
            "RECORD_STORE_CLUSTER_CAPACITY_LOW_WATERMARK_PERCENT",
        )? {
            self.cluster.capacity_low_watermark_percent =
                parse_environment("RECORD_STORE_CLUSTER_CAPACITY_LOW_WATERMARK_PERCENT", value)?;
        }
        if let Some(value) = environment_value(
            environment,
            "RECORD_STORE_CLUSTER_CAPACITY_HIGH_WATERMARK_PERCENT",
        )? {
            self.cluster.capacity_high_watermark_percent = parse_environment(
                "RECORD_STORE_CLUSTER_CAPACITY_HIGH_WATERMARK_PERCENT",
                value,
            )?;
        }
        if let Some(value) = environment_value(
            environment,
            "RECORD_STORE_CLUSTER_CAPACITY_CRITICAL_WATERMARK_PERCENT",
        )? {
            self.cluster.capacity_critical_watermark_percent = parse_environment(
                "RECORD_STORE_CLUSTER_CAPACITY_CRITICAL_WATERMARK_PERCENT",
                value,
            )?;
        }
        if let Some(value) =
            environment_value(environment, "RECORD_STORE_CLUSTER_MOVEMENT_CONCURRENCY")?
        {
            self.cluster.movement_concurrency =
                parse_environment("RECORD_STORE_CLUSTER_MOVEMENT_CONCURRENCY", value)?;
        }
        if let Some(value) = environment_value(
            environment,
            "RECORD_STORE_CLUSTER_MOVEMENT_BYTES_PER_SECOND",
        )? {
            self.cluster.movement_bytes_per_second =
                parse_environment("RECORD_STORE_CLUSTER_MOVEMENT_BYTES_PER_SECOND", value)?;
        }
        if let Some(value) = environment_value(
            environment,
            "RECORD_STORE_CLUSTER_RECONCILE_INTERVAL_SECONDS",
        )? {
            self.cluster.reconcile_interval_seconds =
                parse_environment("RECORD_STORE_CLUSTER_RECONCILE_INTERVAL_SECONDS", value)?;
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_CLUSTER_TLS_CERTIFICATE")?
        {
            self.cluster.tls.certificate_path = Some(PathBuf::from(value));
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_CLUSTER_TLS_PRIVATE_KEY")?
        {
            self.cluster.tls.private_key_path = Some(PathBuf::from(value));
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_CLUSTER_TLS_PEER_CA")? {
            self.cluster.tls.peer_ca_path = Some(PathBuf::from(value));
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_CLUSTER_TLS_CLIENT_CA")? {
            self.cluster.tls.client_ca_path = Some(PathBuf::from(value));
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_CLUSTER_TLS_SERVER_NAME")?
        {
            self.cluster.tls.server_name = Some(value.to_owned());
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_LOG")? {
            self.observability.log_filter = value.to_owned();
        }
        if let Some(value) = environment_value(environment, "RECORD_STORE_LOG_JSON")? {
            self.observability.json = parse_environment("RECORD_STORE_LOG_JSON", value)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;
    use crate::Config;
    use crate::test_support::*;

    #[test]
    fn file_and_environment_overlay_defaults_in_order() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("record-store.toml");
        fs::write(
            &path,
            r#"
                [server]
                s3_bind = "127.0.0.1:7700"

                [storage]
                data_directory = "/srv/record-store"
            "#,
        )
        .expect("write configuration");
        let mut environment = credentials().to_vec();
        environment.push(("RECORD_STORE_API_BIND", "127.0.0.1:7701"));
        environment.push(("RECORD_STORE_LOG", "record_store=debug"));
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
        assert_eq!(
            config.storage.data_directory,
            PathBuf::from("/srv/record-store")
        );
        assert_eq!(config.observability.log_filter, "record_store=debug");
    }

    #[test]
    fn unknown_file_fields_and_invalid_environment_are_rejected() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("record-store.toml");
        fs::write(&path, "[server]\nsecret_backdoor = true\n").expect("write configuration");
        assert!(matches!(
            Config::load_with_environment(Some(&path), credentials()),
            Err(ConfigError::ParseFile { .. })
        ));
        let mut environment = credentials().to_vec();
        environment.push(("RECORD_STORE_S3_BIND", "not-an-address"));
        let error =
            Config::load_with_environment(None, environment).expect_err("invalid environment");
        assert!(error.to_string().contains("RECORD_STORE_S3_BIND"));
        assert!(!error.to_string().contains("test-secret"));
    }

    #[test]
    fn cluster_environment_overrides_are_applied() {
        let config = Config::load_with_environment(
            None,
            [
                ("RECORD_STORE_ROOT_ACCESS_KEY", "root-access"),
                (
                    "RECORD_STORE_ROOT_SECRET_KEY",
                    "root-secret-at-least-sixteen",
                ),
                ("RECORD_STORE_MODE", "cluster"),
                ("RECORD_STORE_RPC_BIND", "0.0.0.0:17603"),
                ("RECORD_STORE_RPC_ADVERTISE", "10.0.1.12:17603"),
                (
                    "RECORD_STORE_CLUSTER_SEEDS",
                    "storage-1:7603, storage-2:7603",
                ),
                ("RECORD_STORE_CLUSTER_JOIN_TOKEN", "recordstorejoin.token"),
                ("RECORD_STORE_CLUSTER_STORAGE_CLASS", "nvme"),
                ("RECORD_STORE_CLUSTER_FAILURE_DOMAIN", "rack=r1,zone=dc1"),
                ("RECORD_STORE_CLUSTER_REPLICATION_FACTOR", "2"),
                ("RECORD_STORE_CLUSTER_CAPACITY_LOW_WATERMARK_PERCENT", "70"),
                ("RECORD_STORE_CLUSTER_CAPACITY_HIGH_WATERMARK_PERCENT", "80"),
                (
                    "RECORD_STORE_CLUSTER_CAPACITY_CRITICAL_WATERMARK_PERCENT",
                    "90",
                ),
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

#[cfg(test)]
mod exhaustive_tests {
    use std::collections::BTreeSet;

    use crate::test_support::credentials;
    use crate::{Config, DeploymentMode};

    /// Every variable the overlay reads, with a distinctive valid value.
    ///
    /// Setting all of them at once is what makes this a contract test rather
    /// than a sampling: an overlay arm that silently stops reading its variable
    /// shows up here as a field that kept its default.
    fn every_variable(directory: &std::path::Path) -> Vec<(&'static str, String)> {
        let mut settings: Vec<(&'static str, String)> = vec![
            ("RECORD_STORE_MODE", "cluster".into()),
            ("RECORD_STORE_S3_BIND", "127.0.0.1:17600".into()),
            ("RECORD_STORE_RPC_BIND", "127.0.0.1:17603".into()),
            ("RECORD_STORE_RPC_ADVERTISE", "node-a:17603".into()),
            ("RECORD_STORE_API_BIND", "127.0.0.1:17601".into()),
            ("RECORD_STORE_SHUTDOWN_TIMEOUT_SECONDS", "45".into()),
            ("RECORD_STORE_STORAGE_DATA_DIRECTORY", "/srv/records".into()),
            (
                "RECORD_STORE_STORAGE_TEMPORARY_DIRECTORY",
                "/srv/records-tmp".into(),
            ),
            ("RECORD_STORE_STORAGE_ENCRYPTION_ENABLED", "true".into()),
            (
                "RECORD_STORE_CREDENTIAL_MASTER_KEY",
                "credential-master-key-at-least-32-bytes".into(),
            ),
            ("RECORD_STORE_ROOT_S3_ENABLED", "false".into()),
            (
                "RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN",
                "system-token-at-least-thirty-two-bytes".into(),
            ),
            (
                "RECORD_STORE_MANAGEMENT_STORAGE_TOKEN",
                "storage-token-at-least-thirty-two-byte".into(),
            ),
            (
                "RECORD_STORE_MANAGEMENT_AUDITOR_TOKEN",
                "auditor-token-at-least-thirty-two-byte".into(),
            ),
            (
                "RECORD_STORE_METRICS_SCRAPE_TOKEN",
                "metrics-token-at-least-thirty-two-byte".into(),
            ),
            ("RECORD_STORE_MAX_CONCURRENT_OPERATIONS", "64".into()),
            ("RECORD_STORE_MAX_HEADER_BYTES", "32768".into()),
            ("RECORD_STORE_WEBHOOK_ALLOW_HTTP", "true".into()),
            ("RECORD_STORE_WEBHOOK_ALLOW_PRIVATE_NETWORKS", "true".into()),
            ("RECORD_STORE_WEBHOOK_TIMEOUT_SECONDS", "9".into()),
            ("RECORD_STORE_WEBHOOK_MAXIMUM_ATTEMPTS", "7".into()),
            ("RECORD_STORE_WEBHOOK_POLL_INTERVAL_SECONDS", "11".into()),
            ("RECORD_STORE_LIFECYCLE_INTERVAL_SECONDS", "600".into()),
            ("RECORD_STORE_LIFECYCLE_BATCH_SIZE", "250".into()),
            ("RECORD_STORE_SHARING_SHARES_ENABLED", "true".into()),
            ("RECORD_STORE_SHARING_EMBEDS_ENABLED", "true".into()),
            ("RECORD_STORE_SHARING_MAXIMUM_LIFETIME_DAYS", "14".into()),
            ("RECORD_STORE_SHARING_REQUIRE_EXPIRATION", "true".into()),
            ("RECORD_STORE_SHARING_REQUIRE_PASSWORD", "true".into()),
            ("RECORD_STORE_SHARING_MAXIMUM_ACCESS_COUNT", "500".into()),
            ("RECORD_STORE_SHARING_TOKEN_PROBES_PER_MINUTE", "30".into()),
            ("RECORD_STORE_SHARING_UNLOCK_LIFETIME_HOURS", "6".into()),
            (
                "RECORD_STORE_SHARING_PASSWORD_ATTEMPTS_PER_MINUTE",
                "12".into(),
            ),
            (
                "RECORD_STORE_SHARING_PREVIEW_TEXT_LIMIT_BYTES",
                "65536".into(),
            ),
            (
                "RECORD_STORE_SHARING_SHARE_BASE_URL",
                "https://share.example".into(),
            ),
            (
                "RECORD_STORE_SHARING_EMBED_BASE_URL",
                "https://embed.example".into(),
            ),
            ("RECORD_STORE_CLUSTER_SEEDS", "node-b:17603".into()),
            ("RECORD_STORE_CLUSTER_JOIN_TOKEN", "join-token".into()),
            ("RECORD_STORE_CLUSTER_STORAGE_CLASS", "standard".into()),
            ("RECORD_STORE_CLUSTER_FAILURE_DOMAIN", "rack=a".into()),
            (
                "RECORD_STORE_CLUSTER_S3_ENDPOINT",
                "https://s3.example".into(),
            ),
            ("RECORD_STORE_CLUSTER_REPLICATION_FACTOR", "3".into()),
            (
                "RECORD_STORE_CLUSTER_CAPACITY_LOW_WATERMARK_PERCENT",
                "40".into(),
            ),
            (
                "RECORD_STORE_CLUSTER_CAPACITY_HIGH_WATERMARK_PERCENT",
                "80".into(),
            ),
            (
                "RECORD_STORE_CLUSTER_CAPACITY_CRITICAL_WATERMARK_PERCENT",
                "95".into(),
            ),
            ("RECORD_STORE_CLUSTER_MOVEMENT_CONCURRENCY", "4".into()),
            (
                "RECORD_STORE_CLUSTER_MOVEMENT_BYTES_PER_SECOND",
                "1048576".into(),
            ),
            (
                "RECORD_STORE_CLUSTER_RECONCILE_INTERVAL_SECONDS",
                "20".into(),
            ),
            (
                "RECORD_STORE_CLUSTER_TLS_CERTIFICATE",
                path(directory, "node.crt"),
            ),
            (
                "RECORD_STORE_CLUSTER_TLS_PRIVATE_KEY",
                path(directory, "node.key"),
            ),
            (
                "RECORD_STORE_CLUSTER_TLS_PEER_CA",
                path(directory, "peer-ca.crt"),
            ),
            (
                "RECORD_STORE_CLUSTER_TLS_CLIENT_CA",
                path(directory, "client-ca.crt"),
            ),
            ("RECORD_STORE_CLUSTER_TLS_SERVER_NAME", "node-a".into()),
            ("RECORD_STORE_LOG", "debug".into()),
            ("RECORD_STORE_LOG_JSON", "true".into()),
        ];
        settings.extend(
            credentials()
                .into_iter()
                .map(|(name, value)| (name, value.to_owned())),
        );
        settings
    }

    /// Creates a placeholder file and returns its path, for settings that name
    /// one. Validation checks that the file is present, not that it parses.
    fn path(directory: &std::path::Path, name: &str) -> String {
        let file = directory.join(name);
        std::fs::write(&file, b"placeholder").expect("write");
        file.to_str().expect("path").to_owned()
    }

    /// Loads a configuration built entirely from the environment.
    fn loaded() -> (tempfile::TempDir, Config) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config = Config::load_with_environment(None, every_variable(directory.path()))
            .expect("every documented variable must be accepted together");
        (directory, config)
    }

    /// The whole overlay applied at once, checked field by field. A variable
    /// that stops being read leaves its field at the default and fails here.
    #[test]
    fn every_environment_variable_reaches_its_setting() {
        let (_directory, config) = loaded();

        assert_eq!(config.server.mode, DeploymentMode::Cluster);
        assert_eq!(config.server.s3_bind.to_string(), "127.0.0.1:17600");
        assert_eq!(config.server.rpc_bind.to_string(), "127.0.0.1:17603");
        assert_eq!(config.server.rpc_advertise.as_deref(), Some("node-a:17603"));
        assert_eq!(config.server.api_bind.to_string(), "127.0.0.1:17601");
        assert_eq!(config.server.shutdown_grace_period_seconds, 45);

        assert_eq!(
            config.storage.data_directory,
            std::path::PathBuf::from("/srv/records")
        );
        assert_eq!(
            config.storage.temporary_directory,
            Some(std::path::PathBuf::from("/srv/records-tmp"))
        );
        assert!(config.storage.encryption_enabled);

        assert!(!config.auth.root_s3_enabled);
        assert!(config.auth.credential_master_key.is_some());
        assert!(config.auth.management_system_token.is_some());
        assert!(config.auth.management_storage_token.is_some());
        assert!(config.auth.management_auditor_token.is_some());
        assert!(config.auth.metrics_scrape_token.is_some());

        assert_eq!(config.limits.maximum_concurrent_operations, 64);
        assert_eq!(config.limits.maximum_header_bytes, 32_768);

        assert!(config.webhooks.allow_http);
        assert!(config.webhooks.allow_private_networks);
        assert_eq!(config.webhooks.request_timeout_seconds, 9);
        assert_eq!(config.webhooks.maximum_attempts, 7);
        assert_eq!(config.webhooks.poll_interval_seconds, 11);

        assert_eq!(config.lifecycle.interval_seconds, 600);
        assert_eq!(config.lifecycle.batch_size, 250);

        assert!(config.sharing.shares_enabled);
        assert!(config.sharing.embeds_enabled);
        assert_eq!(config.sharing.maximum_lifetime_days, 14);
        assert!(config.sharing.require_expiration);
        assert!(config.sharing.require_share_password);
        assert_eq!(config.sharing.maximum_access_count, 500);
        assert_eq!(config.sharing.token_probes_per_minute, 30);
        assert_eq!(config.sharing.unlock_lifetime_hours, 6);
        assert_eq!(config.sharing.password_attempts_per_minute, 12);
        assert_eq!(config.sharing.preview_text_limit_bytes, 65_536);
        assert_eq!(
            config.sharing.share_base_url.as_deref(),
            Some("https://share.example")
        );
        assert_eq!(
            config.sharing.embed_base_url.as_deref(),
            Some("https://embed.example")
        );

        assert_eq!(config.cluster.seeds, vec!["node-b:17603".to_owned()]);
        assert!(config.cluster.join_token.is_some());
        assert_eq!(config.cluster.storage_class, "standard");
        assert_eq!(config.cluster.replication_factor, 3);
        assert_eq!(config.cluster.capacity_low_watermark_percent, 40);
        assert_eq!(config.cluster.capacity_high_watermark_percent, 80);
        assert_eq!(config.cluster.capacity_critical_watermark_percent, 95);
        assert_eq!(config.cluster.movement_concurrency, 4);
        assert_eq!(config.cluster.movement_bytes_per_second, 1_048_576);
        assert_eq!(config.cluster.reconcile_interval_seconds, 20);

        assert_eq!(config.observability.log_filter, "debug");
        assert!(config.observability.json);
    }

    /// The overlay is the deployment contract, so the set of variables it reads
    /// is pinned. A new one added without a test is caught here.
    #[test]
    fn the_documented_variable_set_matches_what_the_overlay_reads() {
        let source = include_str!("environment.rs");
        let read: BTreeSet<&str> = source
            .match_indices("environment_value(environment, \"")
            .filter_map(|(index, marker)| {
                let rest = &source[index + marker.len()..];
                rest.find('"').map(|end| &rest[..end])
            })
            .collect();
        let directory = tempfile::tempdir().expect("temporary directory");
        let covered: BTreeSet<&str> = every_variable(directory.path())
            .into_iter()
            .map(|(name, _)| name)
            .collect();

        let missing: Vec<_> = read.difference(&covered).collect();
        assert!(
            missing.is_empty(),
            "these variables are read but not exercised: {missing:?}"
        );
    }

    /// A configuration built entirely from the environment still has to satisfy
    /// validation; an overlay that produces an unusable deployment is worse than
    /// one that refuses at parse time.
    #[test]
    fn a_fully_environment_driven_configuration_validates() {
        let (_directory, config) = loaded();
        config.validate().expect("validate");
    }

    /// Every numeric setting is parsed rather than defaulted, so a value that is
    /// not a number has to be refused by name.
    #[test]
    fn a_non_numeric_value_is_refused_and_names_the_variable() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut settings = every_variable(directory.path());
        settings.retain(|(name, _)| *name != "RECORD_STORE_MAX_HEADER_BYTES");
        settings.push(("RECORD_STORE_MAX_HEADER_BYTES", "plenty".into()));

        let error = Config::load_with_environment(None, settings)
            .expect_err("a non-numeric byte count must be refused");
        assert!(
            error.to_string().contains("RECORD_STORE_MAX_HEADER_BYTES"),
            "{error}"
        );
    }

    #[test]
    fn an_unparseable_listener_address_is_refused_by_name() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut settings = every_variable(directory.path());
        settings.retain(|(name, _)| *name != "RECORD_STORE_API_BIND");
        settings.push(("RECORD_STORE_API_BIND", "not-an-address".into()));

        let error =
            Config::load_with_environment(None, settings).expect_err("bad address must be refused");
        assert!(
            error.to_string().contains("RECORD_STORE_API_BIND"),
            "{error}"
        );
    }
}
