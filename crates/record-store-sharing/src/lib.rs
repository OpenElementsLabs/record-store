//! Secure, revocable external access to stored objects.
//!
//! OES objects are reachable three ways once this crate is wired in. An
//! administrator previews them inside the console under their own management
//! session; a person opens a *share link*; an application or website loads an
//! *embed link*. Only the first is authenticated in the ordinary sense. The
//! other two are capabilities: unguessable, narrowly scoped, individually
//! revocable, and deliberately unable to express anything beyond reading one
//! object.
//!
//! Nothing here grants listing, writing, deleting, or credential access, and
//! nothing here is a general storage credential. A capability names one logical
//! object and one version policy, and that is the whole of its authority.

mod limiter;
mod model;
mod origin;
mod password;
mod store;
mod ticket;
mod token;

use std::{sync::Arc, time::Duration as StdDuration};

use chrono::{DateTime, Duration, Utc};
use oes_core::{BucketId, BucketName, EmbedLinkId, ObjectKey, PreviewKind, ShareLinkId, VersionId};
use thiserror::Error;

pub use crate::{
    limiter::{RateDecision, RateLimiter},
    model::{
        CapabilityStatus, CapabilityTarget, EmbedDisposition, EmbedLink, ShareLink,
        SharePermission, VersionMode,
    },
    origin::{
        AllowedOrigin, MAXIMUM_ALLOWED_ORIGINS, OriginDecision, evaluate_origin, matching_origin,
    },
    password::{MAXIMUM_PASSWORD_LENGTH, MINIMUM_PASSWORD_LENGTH, PasswordHash},
    store::{AccessRefusal, CapabilityStore, SHARING_SCHEMA_VERSION},
    ticket::{TicketIssuer, UnlockTicket},
    token::{
        CapabilityToken, TOKEN_ENTROPY_BYTES, TOKEN_TEXT_LENGTH, TokenDigest,
        redact_capability_path,
    },
};

/// Stable failure categories for capability operations.
#[derive(Debug, Error)]
pub enum SharingError {
    /// The capability store directory could not be prepared.
    #[error("failed to prepare capability directory: {0}")]
    Directory(#[source] std::io::Error),
    /// The operating system would not provide randomness.
    #[error("secure randomness is unavailable")]
    EntropyUnavailable,
    /// Encryption or key derivation failed.
    #[error("capability cryptography failed")]
    Cryptography,
    /// Hashing a share password failed.
    #[error("share password hashing failed")]
    PasswordHashFailed,
    /// A share password did not satisfy the minimum requirements.
    #[error("invalid share password: {0}")]
    InvalidPassword(String),
    /// An embed origin was malformed or used a disallowed scheme.
    #[error("invalid origin: {0}")]
    InvalidOrigin(String),
    /// A capability request violated a validation rule.
    #[error("invalid capability request: {0}")]
    Invalid(String),
    /// Deployment policy forbids this capability.
    #[error("{0}")]
    PolicyRefused(String),
    /// Two tokens hashed to the same value, which in practice means the store
    /// is being fed a token it has already issued.
    #[error("capability token collision")]
    TokenCollision,
    /// Encoding or decoding a stored record failed.
    #[error("capability encoding failed: {0}")]
    Encoding(#[from] serde_json::Error),
    /// The durable store failed.
    #[error("capability operation '{operation}' failed: {reason}")]
    Database {
        /// What was being attempted.
        operation: &'static str,
        /// The backend's description of the failure.
        reason: String,
    },
    /// A blocking store task failed to run.
    #[error("capability task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

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

/// What a caller must supply to create a share link.
#[derive(Debug, Clone)]
pub struct CreateShareRequest {
    /// Operator-facing name.
    pub label: String,
    /// Owning bucket identifier.
    pub bucket_id: BucketId,
    /// Owning bucket name.
    pub bucket: BucketName,
    /// Target object key.
    pub key: ObjectKey,
    /// Which version the share resolves to.
    pub version: VersionMode,
    /// What the recipient may do.
    pub permission: SharePermission,
    /// Optional expiry.
    pub expires_at: Option<DateTime<Utc>>,
    /// Optional password, in the clear exactly once, on the way to a hash.
    pub password: Option<String>,
    /// Optional strict ceiling on deliveries.
    pub maximum_access_count: Option<u32>,
    /// The management identity creating this share.
    pub created_by: String,
}

/// What a caller must supply to create an embed link.
#[derive(Debug, Clone)]
pub struct CreateEmbedRequest {
    /// Operator-facing name.
    pub label: String,
    /// Owning bucket identifier.
    pub bucket_id: BucketId,
    /// Owning bucket name.
    pub bucket: BucketName,
    /// Target object key.
    pub key: ObjectKey,
    /// Which version the embed resolves to.
    pub version: VersionMode,
    /// Optional expiry.
    pub expires_at: Option<DateTime<Utc>>,
    /// Origins permitted to read the bytes from a browser.
    pub allowed_origins: Vec<String>,
    /// How the bytes are presented.
    pub disposition: EmbedDisposition,
    /// The validated media type of the target object at creation time.
    pub content_type: Option<String>,
    /// The management identity creating this embed.
    pub created_by: String,
}

/// A newly created capability, with its token exposed exactly once here.
#[derive(Debug)]
pub struct IssuedCapability<T> {
    /// The durable descriptor.
    pub link: T,
    /// The secret token. The caller decides whether to disclose it.
    pub token: CapabilityToken,
}

/// What a share lookup discloses before a password has been supplied.
#[derive(Debug)]
pub enum ShareLookup {
    /// Nothing matched, or the capability is no longer usable. The caller must
    /// not distinguish these outward: telling a prober that a token exists but
    /// expired is still telling them the token exists.
    Unavailable(AccessRefusal),
    /// The share exists and is usable, but nothing about the object — not even
    /// its file name — is disclosed until the password is verified.
    PasswordRequired(ShareLinkId),
    /// The share is open.
    Open(Box<ShareLink>),
}

/// Why a password unlock failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnlockFailure {
    /// The token named nothing usable.
    Unavailable(AccessRefusal),
    /// The share has no password, so there is nothing to unlock.
    NotPasswordProtected,
    /// The password was wrong.
    IncorrectPassword,
    /// Too many attempts from this client against this share.
    Throttled {
        /// Seconds the caller should wait.
        retry_after_seconds: u64,
    },
}

/// Why an access was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessDenial {
    /// Nothing matched, or the capability is no longer usable.
    Unavailable(AccessRefusal),
    /// A password is required and was not proven.
    PasswordRequired,
    /// The share does not permit this kind of access.
    NotPermitted,
    /// The request's origin is not on the embed's allowlist.
    OriginDenied,
    /// The caller is being throttled.
    Throttled {
        /// Seconds the caller should wait.
        retry_after_seconds: u64,
    },
}

/// Coordinates capability creation, lookup, authorization, and abuse control.
pub struct SharingService {
    store: CapabilityStore,
    policy: SharingPolicy,
    tickets: TicketIssuer,
    password_attempts: RateLimiter<(ShareLinkId, String)>,
    token_probes: RateLimiter<String>,
}

impl std::fmt::Debug for SharingService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SharingService")
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl SharingService {
    /// Assembles the service from a durable store and a deployment policy.
    #[must_use]
    pub fn new(store: CapabilityStore, policy: SharingPolicy, tickets: TicketIssuer) -> Self {
        let window = policy.abuse_window;
        Self {
            password_attempts: RateLimiter::new(policy.password_attempts_per_window, window, 4_096),
            token_probes: RateLimiter::new(policy.token_probes_per_window, window, 8_192),
            store,
            policy,
            tickets,
        }
    }

    /// Returns the deployment policy in force.
    #[must_use]
    pub const fn policy(&self) -> &SharingPolicy {
        &self.policy
    }

    /// Returns the durable store, for listings and administration.
    #[must_use]
    pub const fn store(&self) -> &CapabilityStore {
        &self.store
    }

    /// Creates a share link and returns its token exactly once.
    pub async fn create_share(
        &self,
        request: CreateShareRequest,
        now: DateTime<Utc>,
    ) -> Result<IssuedCapability<ShareLink>, SharingError> {
        if !self.policy.shares_enabled {
            return Err(SharingError::PolicyRefused(
                "Share links are disabled for this deployment".to_owned(),
            ));
        }
        let label = validated_label(&request.label)?;
        let expires_at = self.validated_expiry(request.expires_at, now)?;
        let password = match request.password.as_deref() {
            Some(password) => Some(PasswordHash::create(password)?),
            None if self.policy.require_share_password => {
                return Err(SharingError::PolicyRefused(
                    "This deployment requires every share link to have a password".to_owned(),
                ));
            }
            None => None,
        };
        if request
            .maximum_access_count
            .is_some_and(|maximum| maximum == 0 || maximum > self.policy.maximum_access_count)
        {
            return Err(SharingError::Invalid(format!(
                "an access limit must be between 1 and {}",
                self.policy.maximum_access_count
            )));
        }
        let token = CapabilityToken::generate()?;
        let link = ShareLink {
            id: ShareLinkId::new(),
            label,
            target: CapabilityTarget {
                bucket_id: request.bucket_id,
                bucket: request.bucket,
                key: request.key,
                version: request.version,
            },
            created_by: request.created_by,
            created_at: now,
            expires_at,
            permission: request.permission,
            password,
            maximum_access_count: request.maximum_access_count,
            access_count: 0,
            revoked_at: None,
            last_accessed_at: None,
        };
        self.store.create_share(link.clone(), &token).await?;
        Ok(IssuedCapability { link, token })
    }

    /// Creates an embed link and returns its token exactly once.
    ///
    /// Embed creation is refused outright for media types that cannot be served
    /// inline safely. An administrator must not be able to turn stored HTML into
    /// an executable document on a trusted origin by pasting one snippet, and
    /// the moment to prevent that is creation rather than delivery.
    pub async fn create_embed(
        &self,
        request: CreateEmbedRequest,
        now: DateTime<Utc>,
    ) -> Result<IssuedCapability<EmbedLink>, SharingError> {
        if !self.policy.embeds_enabled {
            return Err(SharingError::PolicyRefused(
                "Embed links are disabled for this deployment".to_owned(),
            ));
        }
        let label = validated_label(&request.label)?;
        let expires_at = self.validated_expiry(request.expires_at, now)?;
        let content_type = request.content_type.clone().unwrap_or_default();
        let kind = PreviewKind::classify(Some(&content_type));
        let canonical = PreviewKind::canonical_content_type(&content_type);
        match (request.disposition, kind, canonical) {
            (EmbedDisposition::Inline, kind, Some(canonical)) if kind.allows_inline() => {
                if request.allowed_origins.len() > MAXIMUM_ALLOWED_ORIGINS {
                    return Err(SharingError::Invalid(format!(
                        "an embed may list at most {MAXIMUM_ALLOWED_ORIGINS} origins"
                    )));
                }
                let origins = request
                    .allowed_origins
                    .iter()
                    .map(|origin| AllowedOrigin::parse(origin))
                    .collect::<Result<Vec<_>, _>>()?;
                self.finish_embed(
                    label,
                    request,
                    origins,
                    canonical.to_owned(),
                    expires_at,
                    now,
                )
                .await
            }
            (EmbedDisposition::Attachment, _, _) => {
                let origins = request
                    .allowed_origins
                    .iter()
                    .map(|origin| AllowedOrigin::parse(origin))
                    .collect::<Result<Vec<_>, _>>()?;
                // An attachment embed never renders in the host page, so any
                // media type is safe to serve: the browser is being asked to
                // save a file, not to interpret one.
                self.finish_embed(
                    label,
                    request,
                    origins,
                    "application/octet-stream".to_owned(),
                    expires_at,
                    now,
                )
                .await
            }
            (EmbedDisposition::Inline, PreviewKind::UnsafeInline, _) => {
                Err(SharingError::PolicyRefused(format!(
                    "{content_type} can carry active content and cannot be embedded inline. \
                     Create a download embed instead."
                )))
            }
            (EmbedDisposition::Inline, _, _) => Err(SharingError::PolicyRefused(format!(
                "{} cannot be embedded inline safely. Create a download embed instead.",
                if content_type.is_empty() {
                    "An object with no recorded media type"
                } else {
                    content_type.as_str()
                }
            ))),
        }
    }

    async fn finish_embed(
        &self,
        label: String,
        request: CreateEmbedRequest,
        allowed_origins: Vec<AllowedOrigin>,
        content_type: String,
        expires_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Result<IssuedCapability<EmbedLink>, SharingError> {
        let token = CapabilityToken::generate()?;
        let link = EmbedLink {
            id: EmbedLinkId::new(),
            label,
            target: CapabilityTarget {
                bucket_id: request.bucket_id,
                bucket: request.bucket,
                key: request.key,
                version: request.version,
            },
            created_by: request.created_by,
            created_at: now,
            expires_at,
            allowed_origins,
            disposition: request.disposition,
            content_type,
            revoked_at: None,
            last_accessed_at: None,
            access_count: 0,
            updated_at: None,
        };
        self.store.create_embed(link.clone(), &token).await?;
        Ok(IssuedCapability { link, token })
    }

    /// Looks up a share by token, disclosing only what is safe pre-password.
    ///
    /// A ticket from an earlier unlock is accepted here as well as on the byte
    /// routes. Without that, a visitor who has just entered the password — or
    /// who reloads the page afterwards — would be challenged again by the very
    /// request that is meant to show them the file.
    pub async fn look_up_share(
        &self,
        token: &CapabilityToken,
        ticket: Option<&str>,
        client: &str,
        now: DateTime<Utc>,
    ) -> Result<ShareLookup, SharingError> {
        let Some(link) = self.store.resolve_share(token.digest()).await? else {
            // An unknown token is the signal that someone is guessing, so the
            // probe counter is charged here and nowhere else. A visitor using a
            // real link never touches it, however many ranges they fetch.
            let _ = self.token_probes.check(client.to_owned());
            return Ok(ShareLookup::Unavailable(AccessRefusal::Unknown));
        };
        let status = link.status(now);
        if !status.usable() {
            return Ok(ShareLookup::Unavailable(AccessRefusal::NotUsable(status)));
        }
        if link.password_protected()
            && !ticket.is_some_and(|ticket| self.tickets.verify(ticket, link.id, now))
        {
            return Ok(ShareLookup::PasswordRequired(link.id));
        }
        Ok(ShareLookup::Open(Box::new(link)))
    }

    /// Returns whether a client may attempt another unknown token.
    pub fn probe_allowance(&self, client: &str) -> RateDecision {
        self.token_probes.check(client.to_owned())
    }

    /// Verifies a share password and issues a short-lived unlock ticket.
    pub async fn unlock_share(
        &self,
        token: &CapabilityToken,
        password: &str,
        client: &str,
        now: DateTime<Utc>,
    ) -> Result<Result<UnlockTicket, UnlockFailure>, SharingError> {
        let Some(link) = self.store.resolve_share(token.digest()).await? else {
            let _ = self.token_probes.check(client.to_owned());
            return Ok(Err(UnlockFailure::Unavailable(AccessRefusal::Unknown)));
        };
        let status = link.status(now);
        if !status.usable() {
            return Ok(Err(UnlockFailure::Unavailable(AccessRefusal::NotUsable(
                status,
            ))));
        }
        let Some(hash) = link.password.as_ref() else {
            return Ok(Err(UnlockFailure::NotPasswordProtected));
        };
        // Throttling is per share and per client so a public link cannot be
        // used to lock out every other visitor, which an account-wide lockout
        // would happily allow.
        let key = (link.id, client.to_owned());
        if let RateDecision::Throttled {
            retry_after_seconds,
        } = self.password_attempts.check(key.clone())
        {
            return Ok(Err(UnlockFailure::Throttled {
                retry_after_seconds,
            }));
        }
        if !hash.verify(password) {
            return Ok(Err(UnlockFailure::IncorrectPassword));
        }
        self.password_attempts.forget(&key);
        Ok(Ok(self
            .tickets
            .issue(link.id, now + self.policy.unlock_lifetime)))
    }

    /// Authorizes one share content delivery, consuming budget when granted.
    ///
    /// `ticket` is the proof that a password was entered, and is checked before
    /// anything about the object is disclosed. `wants_download` selects which of
    /// the share's two permissions is being exercised.
    pub async fn authorize_share_access(
        &self,
        token: &CapabilityToken,
        ticket: Option<&str>,
        wants_download: bool,
        client: &str,
        now: DateTime<Utc>,
    ) -> Result<Result<ShareLink, AccessDenial>, SharingError> {
        let Some(link) = self.store.resolve_share(token.digest()).await? else {
            let decision = self.token_probes.check(client.to_owned());
            if let RateDecision::Throttled {
                retry_after_seconds,
            } = decision
            {
                return Ok(Err(AccessDenial::Throttled {
                    retry_after_seconds,
                }));
            }
            return Ok(Err(AccessDenial::Unavailable(AccessRefusal::Unknown)));
        };
        let status = link.status(now);
        if !status.usable() {
            return Ok(Err(AccessDenial::Unavailable(AccessRefusal::NotUsable(
                status,
            ))));
        }
        if link.password_protected() {
            let proven = ticket.is_some_and(|ticket| self.tickets.verify(ticket, link.id, now));
            if !proven {
                return Ok(Err(AccessDenial::PasswordRequired));
            }
        }
        let permitted = if wants_download {
            link.permission.allows_download()
        } else {
            link.permission.allows_view()
        };
        if !permitted {
            return Ok(Err(AccessDenial::NotPermitted));
        }
        // Budget is consumed only now, after every other condition has held,
        // and inside a single write transaction that re-checks them.
        match self.store.consume_share_access(link.id, now).await? {
            Ok(updated) => Ok(Ok(updated)),
            Err(refusal) => Ok(Err(AccessDenial::Unavailable(refusal))),
        }
    }

    /// Authorizes one embed byte delivery.
    pub async fn authorize_embed_access(
        &self,
        token: &CapabilityToken,
        presented_origin: Option<&str>,
        client: &str,
        now: DateTime<Utc>,
    ) -> Result<Result<(EmbedLink, OriginDecision), AccessDenial>, SharingError> {
        let Some(link) = self.store.resolve_embed(token.digest()).await? else {
            let decision = self.token_probes.check(client.to_owned());
            if let RateDecision::Throttled {
                retry_after_seconds,
            } = decision
            {
                return Ok(Err(AccessDenial::Throttled {
                    retry_after_seconds,
                }));
            }
            return Ok(Err(AccessDenial::Unavailable(AccessRefusal::Unknown)));
        };
        let status = link.status(now);
        if !status.usable() {
            return Ok(Err(AccessDenial::Unavailable(AccessRefusal::NotUsable(
                status,
            ))));
        }
        let decision = evaluate_origin(&link.allowed_origins, presented_origin);
        if !decision.permits_delivery() {
            return Ok(Err(AccessDenial::OriginDenied));
        }
        self.store.record_embed_access(link.id, now).await?;
        Ok(Ok((link, decision)))
    }

    /// Revokes a share. Revocation takes effect for the next request.
    pub async fn revoke_share(
        &self,
        id: ShareLinkId,
        now: DateTime<Utc>,
    ) -> Result<Option<ShareLink>, SharingError> {
        self.store.revoke_share(id, now).await
    }

    /// Revokes an embed.
    pub async fn revoke_embed(
        &self,
        id: EmbedLinkId,
        now: DateTime<Utc>,
    ) -> Result<Option<EmbedLink>, SharingError> {
        self.store.revoke_embed(id, now).await
    }

    /// Replaces an embed's origin allowlist after validating every entry.
    pub async fn set_embed_origins(
        &self,
        id: EmbedLinkId,
        origins: &[String],
        now: DateTime<Utc>,
    ) -> Result<Option<EmbedLink>, SharingError> {
        if origins.len() > MAXIMUM_ALLOWED_ORIGINS {
            return Err(SharingError::Invalid(format!(
                "an embed may list at most {MAXIMUM_ALLOWED_ORIGINS} origins"
            )));
        }
        let parsed = origins
            .iter()
            .map(|origin| AllowedOrigin::parse(origin))
            .collect::<Result<Vec<_>, _>>()?;
        self.store.set_embed_origins(id, parsed, now).await
    }

    fn validated_expiry(
        &self,
        expires_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Result<Option<DateTime<Utc>>, SharingError> {
        match expires_at {
            Some(expiry) if expiry <= now => Err(SharingError::Invalid(
                "an expiry must be in the future".to_owned(),
            )),
            Some(expiry) => match self.policy.maximum_lifetime {
                Some(maximum) if expiry > now + maximum => {
                    Err(SharingError::PolicyRefused(format!(
                        "this deployment allows a lifetime of at most {} days",
                        maximum.num_days()
                    )))
                }
                _ => Ok(Some(expiry)),
            },
            None if self.policy.require_expiration => Err(SharingError::PolicyRefused(
                "This deployment requires every link to expire".to_owned(),
            )),
            None => Ok(None),
        }
    }
}

/// A shared handle to the sharing service.
pub type SharedSharingService = Arc<SharingService>;

/// Longest operator-facing capability label.
pub const MAXIMUM_LABEL_LENGTH: usize = 120;

fn validated_label(label: &str) -> Result<String, SharingError> {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        return Err(SharingError::Invalid(
            "a link needs a name so it can be recognised later".to_owned(),
        ));
    }
    if trimmed.chars().count() > MAXIMUM_LABEL_LENGTH {
        return Err(SharingError::Invalid(format!(
            "a link name must be at most {MAXIMUM_LABEL_LENGTH} characters"
        )));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(SharingError::Invalid(
            "a link name must not contain control characters".to_owned(),
        ));
    }
    Ok(trimmed.to_owned())
}

/// Resolves the version a capability points at, for a caller that will then read
/// it through the authoritative object service.
#[must_use]
pub const fn pinned_version(target: &CapabilityTarget) -> Option<VersionId> {
    target.version.pinned()
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use tempfile::tempdir;

    use super::*;

    const KEY: &[u8] = b"capability-test-master-key-at-least-32-bytes";

    async fn service(policy: SharingPolicy) -> SharingService {
        let directory = tempdir().expect("temporary directory");
        let store = CapabilityStore::open(directory.path().join("sharing.redb"), KEY)
            .await
            .expect("open store");
        // The directory is intentionally leaked for the lifetime of the test
        // process so the open database file outlives this helper.
        std::mem::forget(directory);
        SharingService::new(store, policy, TicketIssuer::derive(KEY).expect("tickets"))
    }

    fn share_request() -> CreateShareRequest {
        CreateShareRequest {
            label: "Board review".to_owned(),
            bucket_id: BucketId::new(),
            bucket: BucketName::from_str("reports").expect("bucket"),
            key: ObjectKey::new("q1/summary.pdf").expect("key"),
            version: VersionMode::FollowCurrent,
            permission: SharePermission::ViewAndDownload,
            expires_at: None,
            password: None,
            maximum_access_count: None,
            created_by: "management:system-administrator".to_owned(),
        }
    }

    fn embed_request(content_type: &str) -> CreateEmbedRequest {
        CreateEmbedRequest {
            label: "Company website".to_owned(),
            bucket_id: BucketId::new(),
            bucket: BucketName::from_str("assets").expect("bucket"),
            key: ObjectKey::new("brand/logo.png").expect("key"),
            version: VersionMode::FollowCurrent,
            expires_at: None,
            allowed_origins: vec!["https://example.com".to_owned()],
            disposition: EmbedDisposition::Inline,
            content_type: Some(content_type.to_owned()),
            created_by: "management:system-administrator".to_owned(),
        }
    }

    #[tokio::test]
    async fn a_created_share_authorizes_access_and_a_revoked_one_never_does_again() {
        let service = service(SharingPolicy::default()).await;
        let now = Utc::now();
        let issued = service
            .create_share(share_request(), now)
            .await
            .expect("create share");

        let granted = service
            .authorize_share_access(&issued.token, None, false, "10.0.0.1", now)
            .await
            .expect("authorize");
        assert!(granted.is_ok());

        service
            .revoke_share(issued.link.id, now)
            .await
            .expect("revoke");

        let denied = service
            .authorize_share_access(&issued.token, None, false, "10.0.0.1", now)
            .await
            .expect("authorize");
        assert_eq!(
            denied.err(),
            Some(AccessDenial::Unavailable(AccessRefusal::NotUsable(
                CapabilityStatus::Revoked
            )))
        );
    }

    #[tokio::test]
    async fn an_expired_share_stops_working_without_any_administrative_action() {
        let service = service(SharingPolicy::default()).await;
        let now = Utc::now();
        let mut request = share_request();
        request.expires_at = Some(now + Duration::hours(1));
        let issued = service.create_share(request, now).await.expect("create");

        assert!(
            service
                .authorize_share_access(&issued.token, None, false, "10.0.0.1", now)
                .await
                .expect("authorize")
                .is_ok()
        );
        let later = now + Duration::hours(2);
        assert_eq!(
            service
                .authorize_share_access(&issued.token, None, false, "10.0.0.1", later)
                .await
                .expect("authorize")
                .err(),
            Some(AccessDenial::Unavailable(AccessRefusal::NotUsable(
                CapabilityStatus::Expired
            )))
        );
    }

    #[tokio::test]
    async fn an_access_budget_is_a_real_ceiling() {
        let service = service(SharingPolicy::default()).await;
        let now = Utc::now();
        let mut request = share_request();
        request.maximum_access_count = Some(2);
        let issued = service.create_share(request, now).await.expect("create");

        for attempt in 0..2 {
            assert!(
                service
                    .authorize_share_access(&issued.token, None, true, "10.0.0.1", now)
                    .await
                    .expect("authorize")
                    .is_ok(),
                "delivery {attempt} should be permitted"
            );
        }
        assert_eq!(
            service
                .authorize_share_access(&issued.token, None, true, "10.0.0.1", now)
                .await
                .expect("authorize")
                .err(),
            Some(AccessDenial::Unavailable(AccessRefusal::NotUsable(
                CapabilityStatus::Exhausted
            )))
        );
    }

    #[tokio::test]
    async fn concurrent_deliveries_cannot_overspend_an_access_budget() {
        let service = Arc::new(service(SharingPolicy::default()).await);
        let now = Utc::now();
        let mut request = share_request();
        request.maximum_access_count = Some(5);
        let issued = Arc::new(service.create_share(request, now).await.expect("create"));

        let mut handles = Vec::new();
        for _ in 0..32 {
            let service = Arc::clone(&service);
            let issued = Arc::clone(&issued);
            handles.push(tokio::spawn(async move {
                service
                    .authorize_share_access(&issued.token, None, true, "10.0.0.1", now)
                    .await
                    .expect("authorize")
                    .is_ok()
            }));
        }
        let mut granted = 0;
        for handle in handles {
            if handle.await.expect("join") {
                granted += 1;
            }
        }
        assert_eq!(granted, 5, "the ceiling must hold under concurrency");
    }

    #[tokio::test]
    async fn a_password_protected_share_discloses_nothing_before_it_is_unlocked() {
        let service = service(SharingPolicy::default()).await;
        let now = Utc::now();
        let mut request = share_request();
        request.password = Some("open sesame please".to_owned());
        let issued = service.create_share(request, now).await.expect("create");

        match service
            .look_up_share(&issued.token, None, "10.0.0.1", now)
            .await
            .expect("lookup")
        {
            ShareLookup::PasswordRequired(id) => assert_eq!(id, issued.link.id),
            other => panic!("expected a password challenge, got {other:?}"),
        }
        assert_eq!(
            service
                .authorize_share_access(&issued.token, None, false, "10.0.0.1", now)
                .await
                .expect("authorize")
                .err(),
            Some(AccessDenial::PasswordRequired)
        );

        let wrong = service
            .unlock_share(&issued.token, "not the password", "10.0.0.1", now)
            .await
            .expect("unlock");
        assert_eq!(wrong.err(), Some(UnlockFailure::IncorrectPassword));

        let ticket = service
            .unlock_share(&issued.token, "open sesame please", "10.0.0.1", now)
            .await
            .expect("unlock")
            .expect("correct password");
        assert!(
            service
                .authorize_share_access(
                    &issued.token,
                    Some(ticket.as_str()),
                    false,
                    "10.0.0.1",
                    now
                )
                .await
                .expect("authorize")
                .is_ok()
        );

        // The same ticket also opens the descriptor, so a visitor who has just
        // entered the password is not challenged again by the request that is
        // meant to show them the file.
        match service
            .look_up_share(&issued.token, Some(ticket.as_str()), "10.0.0.1", now)
            .await
            .expect("lookup")
        {
            ShareLookup::Open(link) => assert_eq!(link.id, issued.link.id),
            other => panic!("expected an unlocked share, got {other:?}"),
        }
        // A ticket for a different share does not open this one.
        let stranger = service
            .tickets
            .issue(ShareLinkId::new(), now + Duration::hours(1));
        assert!(matches!(
            service
                .look_up_share(&issued.token, Some(stranger.as_str()), "10.0.0.1", now)
                .await
                .expect("lookup"),
            ShareLookup::PasswordRequired(_)
        ));
    }

    #[tokio::test]
    async fn an_unlock_ticket_is_useless_against_a_different_share() {
        let service = service(SharingPolicy::default()).await;
        let now = Utc::now();
        let mut first = share_request();
        first.password = Some("open sesame please".to_owned());
        let first = service.create_share(first, now).await.expect("create");
        let mut second = share_request();
        second.password = Some("a different secret".to_owned());
        let second = service.create_share(second, now).await.expect("create");

        let ticket = service
            .unlock_share(&first.token, "open sesame please", "10.0.0.1", now)
            .await
            .expect("unlock")
            .expect("ticket");
        assert_eq!(
            service
                .authorize_share_access(
                    &second.token,
                    Some(ticket.as_str()),
                    false,
                    "10.0.0.1",
                    now
                )
                .await
                .expect("authorize")
                .err(),
            Some(AccessDenial::PasswordRequired)
        );
    }

    #[tokio::test]
    async fn repeated_password_guesses_are_throttled_per_share_and_client() {
        let policy = SharingPolicy {
            password_attempts_per_window: 3,
            ..SharingPolicy::default()
        };
        let service = service(policy).await;
        let now = Utc::now();
        let mut request = share_request();
        request.password = Some("open sesame please".to_owned());
        let issued = service.create_share(request, now).await.expect("create");

        for _ in 0..3 {
            assert_eq!(
                service
                    .unlock_share(&issued.token, "wrong", "10.0.0.1", now)
                    .await
                    .expect("unlock")
                    .err(),
                Some(UnlockFailure::IncorrectPassword)
            );
        }
        assert!(matches!(
            service
                .unlock_share(&issued.token, "wrong", "10.0.0.1", now)
                .await
                .expect("unlock")
                .err(),
            Some(UnlockFailure::Throttled { .. })
        ));
        // Another visitor is unaffected: a public link must not be lockable.
        assert_eq!(
            service
                .unlock_share(&issued.token, "wrong", "10.0.0.2", now)
                .await
                .expect("unlock")
                .err(),
            Some(UnlockFailure::IncorrectPassword)
        );
    }

    #[tokio::test]
    async fn a_view_only_share_refuses_download_and_the_reverse() {
        let service = service(SharingPolicy::default()).await;
        let now = Utc::now();
        let mut request = share_request();
        request.permission = SharePermission::View;
        let view_only = service.create_share(request, now).await.expect("create");
        assert_eq!(
            service
                .authorize_share_access(&view_only.token, None, true, "10.0.0.1", now)
                .await
                .expect("authorize")
                .err(),
            Some(AccessDenial::NotPermitted)
        );

        let mut request = share_request();
        request.permission = SharePermission::Download;
        let download_only = service.create_share(request, now).await.expect("create");
        assert_eq!(
            service
                .authorize_share_access(&download_only.token, None, false, "10.0.0.1", now)
                .await
                .expect("authorize")
                .err(),
            Some(AccessDenial::NotPermitted)
        );
    }

    #[tokio::test]
    async fn an_unknown_token_is_indistinguishable_from_a_revoked_one_and_is_rate_limited() {
        let policy = SharingPolicy {
            token_probes_per_window: 2,
            ..SharingPolicy::default()
        };
        let service = service(policy).await;
        let now = Utc::now();
        let stranger = CapabilityToken::generate().expect("token");

        for _ in 0..2 {
            assert_eq!(
                service
                    .authorize_share_access(&stranger, None, false, "10.0.0.9", now)
                    .await
                    .expect("authorize")
                    .err(),
                Some(AccessDenial::Unavailable(AccessRefusal::Unknown))
            );
        }
        assert!(matches!(
            service
                .authorize_share_access(&stranger, None, false, "10.0.0.9", now)
                .await
                .expect("authorize")
                .err(),
            Some(AccessDenial::Throttled { .. })
        ));
    }

    #[tokio::test]
    async fn a_valid_share_is_never_charged_against_the_probe_limiter() {
        let policy = SharingPolicy {
            token_probes_per_window: 2,
            ..SharingPolicy::default()
        };
        let service = service(policy).await;
        let now = Utc::now();
        let issued = service
            .create_share(share_request(), now)
            .await
            .expect("create");
        // A media player seeking through a file issues far more requests than
        // the probe allowance; none of them may be treated as abuse.
        for _ in 0..50 {
            assert!(
                service
                    .authorize_share_access(&issued.token, None, false, "10.0.0.5", now)
                    .await
                    .expect("authorize")
                    .is_ok()
            );
        }
    }

    #[tokio::test]
    async fn embeds_reject_active_content_at_creation() {
        let service = service(SharingPolicy::default()).await;
        let now = Utc::now();
        for content_type in ["text/html", "image/svg+xml", "application/xml"] {
            let error = service
                .create_embed(embed_request(content_type), now)
                .await
                .expect_err("active content must be refused");
            assert!(matches!(error, SharingError::PolicyRefused(_)));
        }
        assert!(
            service
                .create_embed(embed_request("image/png"), now)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn an_embed_serves_a_permitted_origin_and_refuses_an_unlisted_one() {
        let service = service(SharingPolicy::default()).await;
        let now = Utc::now();
        let issued = service
            .create_embed(embed_request("image/png"), now)
            .await
            .expect("create embed");

        let (_, decision) = service
            .authorize_embed_access(&issued.token, Some("https://example.com"), "10.0.0.1", now)
            .await
            .expect("authorize")
            .expect("granted");
        assert_eq!(decision, OriginDecision::Allowed);

        assert_eq!(
            service
                .authorize_embed_access(&issued.token, Some("https://evil.test"), "10.0.0.1", now)
                .await
                .expect("authorize")
                .err(),
            Some(AccessDenial::OriginDenied)
        );

        // A non-browser client presents no origin and is still served, but
        // without a CORS grant.
        let (_, decision) = service
            .authorize_embed_access(&issued.token, None, "10.0.0.1", now)
            .await
            .expect("authorize")
            .expect("granted");
        assert_eq!(decision, OriginDecision::NoOriginPresented);
    }

    #[tokio::test]
    async fn an_embed_s_origins_can_be_narrowed_and_every_entry_is_validated() {
        let service = service(SharingPolicy::default()).await;
        let now = Utc::now();
        let issued = service
            .create_embed(embed_request("image/png"), now)
            .await
            .expect("create embed");

        assert!(
            service
                .set_embed_origins(issued.link.id, &["javascript:alert(1)".to_owned()], now)
                .await
                .is_err()
        );
        let updated = service
            .set_embed_origins(issued.link.id, &["https://app.example.com".to_owned()], now)
            .await
            .expect("update")
            .expect("embed");
        assert_eq!(updated.allowed_origins.len(), 1);
        assert_eq!(
            service
                .authorize_embed_access(&issued.token, Some("https://example.com"), "10.0.0.1", now)
                .await
                .expect("authorize")
                .err(),
            Some(AccessDenial::OriginDenied)
        );
    }

    #[tokio::test]
    async fn a_revoked_embed_stops_serving_bytes_immediately() {
        let service = service(SharingPolicy::default()).await;
        let now = Utc::now();
        let issued = service
            .create_embed(embed_request("image/png"), now)
            .await
            .expect("create embed");
        service
            .revoke_embed(issued.link.id, now)
            .await
            .expect("revoke");
        assert_eq!(
            service
                .authorize_embed_access(&issued.token, Some("https://example.com"), "10.0.0.1", now)
                .await
                .expect("authorize")
                .err(),
            Some(AccessDenial::Unavailable(AccessRefusal::NotUsable(
                CapabilityStatus::Revoked
            )))
        );
    }

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

    #[tokio::test]
    async fn a_capability_url_can_be_copied_again_by_an_authorized_administrator() {
        let service = service(SharingPolicy::default()).await;
        let now = Utc::now();
        let issued = service
            .create_share(share_request(), now)
            .await
            .expect("create");
        let revealed = service
            .store()
            .reveal_share_token(issued.link.id)
            .await
            .expect("reveal")
            .expect("token");
        assert_eq!(revealed.expose(), issued.token.expose());
    }

    #[tokio::test]
    async fn a_deleted_share_is_gone_from_every_index() {
        let service = service(SharingPolicy::default()).await;
        let now = Utc::now();
        let request = share_request();
        let bucket_id = request.bucket_id;
        let key = request.key.clone();
        let issued = service.create_share(request, now).await.expect("create");

        assert_eq!(
            service
                .store()
                .list_shares_for_object(bucket_id, &key)
                .await
                .expect("list")
                .len(),
            1
        );
        assert!(
            service
                .store()
                .delete_share(issued.link.id)
                .await
                .expect("delete")
        );
        assert!(
            service
                .store()
                .list_shares_for_object(bucket_id, &key)
                .await
                .expect("list")
                .is_empty()
        );
        assert!(
            service
                .store()
                .resolve_share(issued.token.digest())
                .await
                .expect("resolve")
                .is_none()
        );
    }

    #[tokio::test]
    async fn capabilities_are_listed_only_against_their_own_object() {
        let service = service(SharingPolicy::default()).await;
        let now = Utc::now();
        let bucket_id = BucketId::new();
        for key in ["reports/a.pdf", "reports/a.pdf.backup", "reports/b.pdf"] {
            let mut request = share_request();
            request.bucket_id = bucket_id;
            request.key = ObjectKey::new(key).expect("key");
            service.create_share(request, now).await.expect("create");
        }
        let listed = service
            .store()
            .list_shares_for_object(bucket_id, &ObjectKey::new("reports/a.pdf").expect("key"))
            .await
            .expect("list");
        assert_eq!(listed.len(), 1, "a key prefix must not match a longer key");
        assert_eq!(listed[0].target.key.as_str(), "reports/a.pdf");
    }

    #[tokio::test]
    async fn a_pinned_share_records_the_exact_version_it_was_created_for() {
        let service = service(SharingPolicy::default()).await;
        let now = Utc::now();
        let version = VersionId::new();
        let mut request = share_request();
        request.version = VersionMode::Pinned {
            version_id: version,
        };
        let issued = service.create_share(request, now).await.expect("create");
        assert_eq!(pinned_version(&issued.link.target), Some(version));

        let resolved = service
            .store()
            .resolve_share(issued.token.digest())
            .await
            .expect("resolve")
            .expect("share");
        assert_eq!(resolved.target.version.pinned(), Some(version));
    }

    #[tokio::test]
    async fn labels_are_validated_rather_than_stored_as_typed() {
        let service = service(SharingPolicy::default()).await;
        let now = Utc::now();
        for label in ["", "   ", "with\u{0}control"] {
            let mut request = share_request();
            request.label = label.to_owned();
            assert!(
                service.create_share(request, now).await.is_err(),
                "accepted label {label:?}"
            );
        }
        let mut request = share_request();
        request.label = "  Board review  ".to_owned();
        let issued = service.create_share(request, now).await.expect("create");
        assert_eq!(issued.link.label, "Board review");
    }
}
