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
