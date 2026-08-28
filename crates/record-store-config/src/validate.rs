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
