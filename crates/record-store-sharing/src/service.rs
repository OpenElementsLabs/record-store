use chrono::{DateTime, Utc};
use record_store_core::{EmbedLinkId, PreviewKind, ShareLinkId};

use crate::support::validated_label;
use crate::*;

/// Coordinates capability creation, lookup, authorization, and abuse control.
pub struct SharingService {
    pub(crate) store: CapabilityStore,
    pub(crate) policy: SharingPolicy,
    pub(crate) tickets: TicketIssuer,
    pub(crate) password_attempts: RateLimiter<(ShareLinkId, String)>,
    pub(crate) token_probes: RateLimiter<String>,
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
