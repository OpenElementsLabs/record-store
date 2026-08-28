//! Configuration loading, environment overrides, secret redaction, and validation.

use std::fmt::Debug;

use serde::Deserialize;

/// Deployment policy for external object-access capabilities.
///
/// Every value here narrows what an administrator may create. None of them are
/// enforcement on their own: the capability service re-checks each one, and the
/// public delivery routes re-check revocation and expiry per request. These
/// settings exist so an operator can make a whole deployment stricter than its
/// most careless administrator.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharingConfig {
    /// Whether share links may be created.
    pub shares_enabled: bool,
    /// Whether embed links may be created.
    pub embeds_enabled: bool,
    /// Longest lifetime a new capability may be given, in days.
    ///
    /// Zero means no ceiling, which is a deliberate opt-in rather than the
    /// default: a capability that never expires is one an operator has to keep
    /// track of forever.
    pub maximum_lifetime_days: u32,
    /// Require every new capability to carry an expiry.
    pub require_expiration: bool,
    /// Require every new share link to carry a password.
    pub require_share_password: bool,
    /// Largest access budget a share may be given.
    pub maximum_access_count: u32,
    /// Failed password attempts permitted per share, per client, per window.
    pub password_attempts_per_minute: u32,
    /// Unknown-token lookups permitted per client, per window.
    pub token_probes_per_minute: u32,
    /// How long a share password unlock remains valid, in hours.
    pub unlock_lifetime_hours: u32,
    /// Largest slice of a text or JSON object the console preview will read.
    ///
    /// The console shows the first slice and says so. Nothing about the stored
    /// object changes, and the full bytes remain one download away.
    pub preview_text_limit_bytes: u64,
    /// Public base URL that share links are built from.
    ///
    /// A share link is a page a person opens, so this is the console's public
    /// address. When it is unset the console completes the link against the
    /// origin the administrator is already using, which is right for a
    /// single-origin deployment and wrong behind a rewriting proxy.
    pub share_base_url: Option<String>,
    /// Public base URL that embed links are built from.
    ///
    /// An embed is pasted into somebody else's page and serves object bytes, so
    /// it is published on the S3-compatible storage endpoint rather than on the
    /// console. Keeping the two apart is what lets a deployment expose storage
    /// to the internet while the management plane stays closed.
    ///
    /// When unset this falls back to the advertised S3 endpoint, and then to the
    /// S3 listener address — useful for development, and something a production
    /// deployment behind a proxy or a separate hostname must set explicitly.
    pub embed_base_url: Option<String>,
}

impl Default for SharingConfig {
    fn default() -> Self {
        Self {
            shares_enabled: true,
            embeds_enabled: true,
            maximum_lifetime_days: 365,
            require_expiration: false,
            require_share_password: false,
            maximum_access_count: 10_000,
            password_attempts_per_minute: 10,
            token_probes_per_minute: 60,
            unlock_lifetime_hours: 12,
            preview_text_limit_bytes: 1024 * 1024,
            share_base_url: None,
            embed_base_url: None,
        }
    }
}

impl SharingConfig {
    /// Returns validation problems with the sharing policy.
    pub(crate) fn issues(&self) -> Vec<String> {
        let mut issues = Vec::new();
        if self.maximum_lifetime_days > 3_650 {
            issues.push("sharing.maximum_lifetime_days must be at most 3650".to_owned());
        }
        if self.maximum_access_count == 0 || self.maximum_access_count > 1_000_000 {
            issues.push("sharing.maximum_access_count must be between 1 and 1000000".to_owned());
        }
        if self.password_attempts_per_minute == 0 || self.password_attempts_per_minute > 1_000 {
            issues
                .push("sharing.password_attempts_per_minute must be between 1 and 1000".to_owned());
        }
        if self.token_probes_per_minute == 0 || self.token_probes_per_minute > 100_000 {
            issues.push("sharing.token_probes_per_minute must be between 1 and 100000".to_owned());
        }
        if self.unlock_lifetime_hours == 0 || self.unlock_lifetime_hours > 168 {
            issues.push("sharing.unlock_lifetime_hours must be between 1 and 168".to_owned());
        }
        if self.preview_text_limit_bytes < 1_024
            || self.preview_text_limit_bytes > 64 * 1_024 * 1_024
        {
            issues.push(
                "sharing.preview_text_limit_bytes must be between 1024 and 67108864".to_owned(),
            );
        }
        for (name, value) in [
            ("sharing.share_base_url", &self.share_base_url),
            ("sharing.embed_base_url", &self.embed_base_url),
        ] {
            if let Some(base) = value {
                let trimmed = base.trim();
                if !(trimmed.starts_with("https://") || trimmed.starts_with("http://"))
                    || trimmed.len() > 512
                    || trimmed.contains(char::is_whitespace)
                {
                    issues.push(format!("{name} must be an absolute http or https URL"));
                }
            }
        }
        issues
    }

    /// Returns the share base URL without a trailing slash.
    #[must_use]
    pub fn normalized_share_base_url(&self) -> Option<String> {
        normalize_base_url(self.share_base_url.as_deref())
    }

    /// Returns the embed base URL without a trailing slash, if one was set.
    #[must_use]
    pub fn normalized_embed_base_url(&self) -> Option<String> {
        normalize_base_url(self.embed_base_url.as_deref())
    }
}

pub(crate) fn normalize_base_url(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.trim_end_matches('/').to_owned())
}
