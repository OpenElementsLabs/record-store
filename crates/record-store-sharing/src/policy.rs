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

#[cfg(test)]
mod tests {

    use chrono::{Duration, Utc};

    use super::*;
    use crate::test_support::*;
    use crate::*;

    #[tokio::test]
    async fn deployment_policy_bounds_lifetime_and_can_require_expiry_and_passwords() {
        let policy = SharingPolicy {
            maximum_lifetime: Some(Duration::days(7)),
            require_expiration: true,
            require_share_password: true,
            ..SharingPolicy::default()
        };
        let service = service(policy).await;
        let now = Utc::now();

        assert!(matches!(
            service.create_share(share_request(), now).await,
            Err(SharingError::PolicyRefused(_))
        ));

        let mut request = share_request();
        request.expires_at = Some(now + Duration::days(30));
        request.password = Some("open sesame please".to_owned());
        assert!(matches!(
            service.create_share(request, now).await,
            Err(SharingError::PolicyRefused(_))
        ));

        let mut request = share_request();
        request.expires_at = Some(now + Duration::days(3));
        assert!(matches!(
            service.create_share(request, now).await,
            Err(SharingError::PolicyRefused(_))
        ));

        let mut request = share_request();
        request.expires_at = Some(now + Duration::days(3));
        request.password = Some("open sesame please".to_owned());
        assert!(service.create_share(request, now).await.is_ok());
    }

    #[tokio::test]
    async fn disabling_sharing_refuses_creation_rather_than_hiding_the_button() {
        let policy = SharingPolicy {
            shares_enabled: false,
            embeds_enabled: false,
            ..SharingPolicy::default()
        };
        let service = service(policy).await;
        let now = Utc::now();
        assert!(matches!(
            service.create_share(share_request(), now).await,
            Err(SharingError::PolicyRefused(_))
        ));
        assert!(matches!(
            service.create_embed(embed_request("image/png"), now).await,
            Err(SharingError::PolicyRefused(_))
        ));
    }
}
