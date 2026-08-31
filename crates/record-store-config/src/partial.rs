//! Configuration loading, environment overrides, secret redaction, and validation.

use std::{fmt::Debug, net::SocketAddr, path::PathBuf};

use serde::Deserialize;

use crate::*;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PartialConfig {
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
    pub(crate) fn apply(self, mut target: Config) -> Config {
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
pub(crate) struct PartialServerConfig {
    mode: Option<DeploymentMode>,
    s3_bind: Option<SocketAddr>,
    api_bind: Option<SocketAddr>,
    rpc_bind: Option<SocketAddr>,
    rpc_advertise: Option<String>,
    shutdown_grace_period_seconds: Option<u64>,
}

impl PartialServerConfig {
    pub(crate) fn apply(self, target: &mut ServerConfig) {
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
pub(crate) struct PartialClusterConfig {
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
    pub(crate) fn apply(self, target: &mut ClusterConfig) {
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
pub(crate) struct PartialStorageConfig {
    data_directory: Option<PathBuf>,
    temporary_directory: Option<PathBuf>,
    encryption_enabled: Option<bool>,
    /// Additional drives this node serves.
    ///
    /// A list has no environment-variable form, so a file is the only way to
    /// declare one. Without this field the whole section is rejected as an
    /// unknown key, which is how it should fail if it is ever removed.
    devices: Option<Vec<StorageDeviceConfig>>,
}

impl PartialStorageConfig {
    pub(crate) fn apply(self, target: &mut StorageConfig) {
        if let Some(value) = self.data_directory {
            target.data_directory = value;
        }
        if let Some(value) = self.temporary_directory {
            target.temporary_directory = Some(value);
        }
        if let Some(value) = self.encryption_enabled {
            target.encryption_enabled = value;
        }
        if let Some(value) = self.devices {
            target.devices = value;
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PartialAuthConfig {
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
    pub(crate) fn apply(self, target: &mut AuthConfig) {
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
pub(crate) struct PartialLimitsConfig {
    maximum_concurrent_operations: Option<usize>,
    maximum_custom_metadata_entries: Option<usize>,
    maximum_custom_metadata_bytes: Option<usize>,
    maximum_header_bytes: Option<usize>,
}

impl PartialLimitsConfig {
    pub(crate) fn apply(self, target: &mut LimitsConfig) {
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
pub(crate) struct PartialWebhookConfig {
    allow_http: Option<bool>,
    allow_private_networks: Option<bool>,
    request_timeout_seconds: Option<u64>,
    maximum_attempts: Option<u32>,
    poll_interval_seconds: Option<u64>,
}

impl PartialWebhookConfig {
    pub(crate) fn apply(self, target: &mut WebhookConfig) {
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
pub(crate) struct PartialLifecycleConfig {
    interval_seconds: Option<u64>,
    batch_size: Option<usize>,
}

impl PartialLifecycleConfig {
    pub(crate) fn apply(self, target: &mut LifecycleConfig) {
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
pub(crate) struct PartialSharingConfig {
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
    pub(crate) fn apply(self, target: &mut SharingConfig) {
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
pub(crate) struct PartialObservabilityConfig {
    log_filter: Option<String>,
    json: Option<bool>,
}

impl PartialObservabilityConfig {
    pub(crate) fn apply(self, target: &mut ObservabilityConfig) {
        if let Some(value) = self.log_filter {
            target.log_filter = value;
        }
        if let Some(value) = self.json {
            target.json = value;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::test_support::credentials;
    use crate::{Config, DeploymentMode};

    /// A configuration file that populates every section and every field.
    ///
    /// The environment overlay has its own exhaustive test; this is the same
    /// contract from the file side, so a field that stops being read from TOML
    /// leaves its default and fails an assertion below.
    fn complete_document(directory: &Path) -> String {
        let file = |name: &str| {
            let path = directory.join(name);
            std::fs::write(&path, b"placeholder").expect("write");
            path.to_str().expect("path").to_owned()
        };
        format!(
            r#"
[server]
mode = "cluster"
api_bind = "127.0.0.1:18601"
rpc_bind = "127.0.0.1:18603"
rpc_advertise = "node-a:18603"
shutdown_grace_period_seconds = 33

[storage]
data_directory = "/srv/from-file"
temporary_directory = "/srv/from-file-tmp"
encryption_enabled = true

[auth]
root_access_key = "file-access"
root_secret_key = "file-secret-at-least-sixteen"
credential_master_key = "credential-master-key-at-least-32-bytes"
management_system_token = "system-token-at-least-thirty-two-bytes"
management_storage_token = "storage-token-at-least-thirty-two-byte"
management_auditor_token = "auditor-token-at-least-thirty-two-byte"
metrics_scrape_token = "metrics-token-at-least-thirty-two-byte"

[limits]
maximum_concurrent_operations = 48
maximum_custom_metadata_entries = 12
maximum_custom_metadata_bytes = 3072
maximum_header_bytes = 24576

[webhooks]
allow_http = true
allow_private_networks = true
request_timeout_seconds = 8
maximum_attempts = 6
poll_interval_seconds = 13

[lifecycle]
interval_seconds = 450
batch_size = 175

[sharing]
shares_enabled = true
embeds_enabled = true
maximum_lifetime_days = 21
require_expiration = true
require_share_password = true
maximum_access_count = 250
password_attempts_per_minute = 9
token_probes_per_minute = 21
unlock_lifetime_hours = 5
preview_text_limit_bytes = 32768
share_base_url = "https://file-share.example"
embed_base_url = "https://file-embed.example"

[cluster]
seeds = ["node-b:18603", "node-c:18603"]
join_token = "file-join-token"
storage_class = "standard"
failure_domain = "rack=b"
replication_factor = 3
capacity_low_watermark_percent = 35
capacity_high_watermark_percent = 75
capacity_critical_watermark_percent = 92
consensus_heartbeat_millis = 300
election_timeout_min_millis = 1200
election_timeout_max_millis = 2400
snapshot_logs_threshold = 4096
retained_logs = 1024
movement_concurrency = 5
movement_bytes_per_second = 2097152
reconcile_interval_seconds = 25

[cluster.tls]
certificate_path = "{certificate}"
private_key_path = "{key}"
peer_ca_path = "{peer_ca}"
client_ca_path = "{client_ca}"
server_name = "node-a"

[observability]
log_filter = "trace"
json = true
"#,
            certificate = file("node.crt"),
            key = file("node.key"),
            peer_ca = file("peer-ca.crt"),
            client_ca = file("client-ca.crt"),
        )
    }

    fn from_file() -> (tempfile::TempDir, Config) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("record-store.toml");
        std::fs::write(&path, complete_document(directory.path())).expect("write config");
        let config = Config::load_with_environment(Some(&path), Vec::<(String, String)>::new())
            .expect("a complete document must load");
        (directory, config)
    }

    #[test]
    fn every_documented_setting_can_be_supplied_from_a_file() {
        let (_directory, config) = from_file();

        assert_eq!(config.server.mode, DeploymentMode::Cluster);
        assert_eq!(config.server.api_bind.to_string(), "127.0.0.1:18601");
        assert_eq!(config.server.rpc_bind.to_string(), "127.0.0.1:18603");
        assert_eq!(config.server.rpc_advertise.as_deref(), Some("node-a:18603"));
        assert_eq!(config.server.shutdown_grace_period_seconds, 33);

        assert_eq!(
            config.storage.data_directory,
            std::path::PathBuf::from("/srv/from-file")
        );
        assert!(config.storage.encryption_enabled);

        assert_eq!(config.auth.root_access_key.as_deref(), Some("file-access"));
        assert!(config.auth.metrics_scrape_token.is_some());

        assert_eq!(config.limits.maximum_concurrent_operations, 48);
        assert_eq!(config.limits.maximum_custom_metadata_entries, 12);
        assert_eq!(config.limits.maximum_custom_metadata_bytes, 3_072);
        assert_eq!(config.limits.maximum_header_bytes, 24_576);

        assert_eq!(config.webhooks.request_timeout_seconds, 8);
        assert_eq!(config.webhooks.maximum_attempts, 6);
        assert_eq!(config.webhooks.poll_interval_seconds, 13);

        assert_eq!(config.lifecycle.interval_seconds, 450);
        assert_eq!(config.lifecycle.batch_size, 175);

        assert_eq!(config.sharing.maximum_lifetime_days, 21);
        assert_eq!(config.sharing.maximum_access_count, 250);
        assert_eq!(config.sharing.unlock_lifetime_hours, 5);
        assert_eq!(config.sharing.preview_text_limit_bytes, 32_768);

        assert_eq!(config.cluster.seeds.len(), 2);
        assert_eq!(config.cluster.replication_factor, 3);
        assert_eq!(config.cluster.capacity_low_watermark_percent, 35);
        assert_eq!(config.cluster.consensus_heartbeat_millis, 300);
        assert_eq!(config.cluster.election_timeout_min_millis, 1_200);
        assert_eq!(config.cluster.election_timeout_max_millis, 2_400);
        assert_eq!(config.cluster.snapshot_logs_threshold, 4_096);
        assert_eq!(config.cluster.retained_logs, 1_024);
        assert_eq!(config.cluster.movement_concurrency, 5);
        assert_eq!(config.cluster.movement_bytes_per_second, 2_097_152);
        assert_eq!(config.cluster.reconcile_interval_seconds, 25);
        assert_eq!(config.cluster.tls.server_name.as_deref(), Some("node-a"));

        assert_eq!(config.observability.log_filter, "trace");
        assert!(config.observability.json);
    }

    #[test]
    fn a_configuration_built_entirely_from_a_file_validates() {
        let (_directory, config) = from_file();
        config.validate().expect("validate");
    }

    /// The environment is the last word, so a value present in both places must
    /// resolve to the environment's. Getting this backwards would make an
    /// operator's override silently ineffective.
    #[test]
    fn the_environment_overrides_the_file_field_by_field() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("record-store.toml");
        std::fs::write(&path, complete_document(directory.path())).expect("write config");

        let mut environment: Vec<(&str, String)> = vec![
            ("RECORD_STORE_API_BIND", "127.0.0.1:19601".into()),
            ("RECORD_STORE_LOG", "warn".into()),
            ("RECORD_STORE_LIFECYCLE_BATCH_SIZE", "1".into()),
        ];
        environment.extend(
            credentials()
                .into_iter()
                .map(|(name, value)| (name, value.to_owned())),
        );

        let config = Config::load_with_environment(Some(&path), environment).expect("load");
        assert_eq!(config.server.api_bind.to_string(), "127.0.0.1:19601");
        assert_eq!(config.observability.log_filter, "warn");
        assert_eq!(config.lifecycle.batch_size, 1);
        assert_eq!(
            config.lifecycle.interval_seconds, 450,
            "a setting the environment does not mention keeps the file's value"
        );
    }

    /// A section left out entirely must fall back to defaults rather than
    /// failing, so a minimal file stays a supported way to run.
    #[test]
    fn omitted_sections_fall_back_to_defaults() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("minimal.toml");
        std::fs::write(&path, "[observability]\njson = true\n").expect("write config");

        let config = Config::load_with_environment(Some(&path), credentials()).expect("load");
        assert!(config.observability.json);
        assert_eq!(
            config.lifecycle,
            crate::LifecycleConfig::default(),
            "an omitted section must keep its defaults"
        );
    }

    #[test]
    fn a_file_that_does_not_parse_is_reported_rather_than_ignored() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("broken.toml");
        std::fs::write(&path, "[server\nmode = ").expect("write config");

        assert!(Config::load_with_environment(Some(&path), credentials()).is_err());
    }

    #[test]
    fn a_missing_configuration_file_is_reported_rather_than_silently_skipped() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("absent.toml");
        assert!(Config::load_with_environment(Some(&path), credentials()).is_err());
    }
}
