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
