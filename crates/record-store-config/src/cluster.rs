//! Configuration loading, environment overrides, secret redaction, and validation.

use std::{fmt::Debug, path::PathBuf};

use serde::Deserialize;

use crate::*;

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
    pub(crate) fn issues(&self, mode: DeploymentMode) -> Vec<String> {
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
