use std::time::Duration as StdDuration;

use chrono::Duration;

/// Deployment-wide limits on what a capability may be created with.
///
/// Bucket-scoped overrides are a deliberate future extension: every rule here is
/// evaluated against one target, so a per-bucket layer becomes a resolution step
/// in front of this struct rather than a change to any decision below it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharingPolicy {
    /// Whether share links may be created at all.
    pub shares_enabled: bool,
    /// Whether embed links may be created at all.
    pub embeds_enabled: bool,
    /// Longest lifetime a new capability may be given.
    pub maximum_lifetime: Option<Duration>,
    /// Whether every new capability must carry an expiry.
    pub require_expiration: bool,
    /// Whether every new share must carry a password.
    pub require_share_password: bool,
    /// Largest access budget a share may be given.
    pub maximum_access_count: u32,
    /// Failed password attempts permitted per share, per client, per window.
    pub password_attempts_per_window: u32,
    /// Unknown-token lookups permitted per client, per window.
    pub token_probes_per_window: u32,
    /// Window applied to both abuse counters.
    pub abuse_window: StdDuration,
    /// How long a password unlock stays valid before it must be re-entered.
    pub unlock_lifetime: Duration,
}

impl Default for SharingPolicy {
    fn default() -> Self {
        Self {
            shares_enabled: true,
            embeds_enabled: true,
            // A year is long enough for an asset URL on a website and short
            // enough that a forgotten capability eventually stops working.
            maximum_lifetime: Some(Duration::days(365)),
            require_expiration: false,
            require_share_password: false,
            maximum_access_count: 10_000,
            password_attempts_per_window: 10,
            token_probes_per_window: 60,
            abuse_window: StdDuration::from_secs(60),
            unlock_lifetime: Duration::hours(12),
        }
    }
}
