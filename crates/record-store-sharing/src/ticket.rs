//! Proof that a share password has already been entered.
//!
//! A password-protected share still has to serve bytes over many requests — a
//! page load, an image, a media player's ranges — and re-prompting for each one
//! is not a product. The alternative of remembering unlocked visitors on the
//! server would turn a public read path into per-visitor durable state, which is
//! both a scaling problem and a privacy one.
//!
//! A ticket instead carries its own proof: the share it unlocks and the instant
//! it stops working, authenticated by a key only Record Store holds. It grants nothing on
//! its own — every request it accompanies still re-reads the share and re-checks
//! revocation, expiry, permission, and budget — so a stolen ticket is worth
//! exactly as much as the link it was issued against, and not one request more
//! after that link is revoked.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use record_store_core::ShareLinkId;
use sha2::Sha256;
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::SharingError;

type HmacSha256 = Hmac<Sha256>;

/// A short-lived unlock proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnlockTicket(String);

impl UnlockTicket {
    /// Returns the ticket text, suitable for a cookie value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the ticket, returning its text.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

/// Mints and checks unlock tickets.
#[derive(Clone)]
pub struct TicketIssuer {
    key: std::sync::Arc<Zeroizing<[u8; 32]>>,
}

impl std::fmt::Debug for TicketIssuer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("TicketIssuer(<redacted>)")
    }
}

impl TicketIssuer {
    /// Derives the signing key from the deployment's key material.
    ///
    /// Derived rather than reused directly so that this key is unrelated to the
    /// one protecting stored tokens: compromising either must not hand over the
    /// other.
    pub fn derive(material: &[u8]) -> Result<Self, SharingError> {
        let derivation = hkdf::Hkdf::<Sha256>::new(Some(b"capability-store-v1"), material);
        let mut key = [0_u8; 32];
        derivation
            .expand(b"share-unlock-ticket-key", &mut key)
            .map_err(|_| SharingError::Cryptography)?;
        Ok(Self {
            key: std::sync::Arc::new(Zeroizing::new(key)),
        })
    }

    /// Issues a ticket for one share, valid until `expires_at`.
    #[must_use]
    pub fn issue(&self, share: ShareLinkId, expires_at: DateTime<Utc>) -> UnlockTicket {
        let expiry = expires_at.timestamp();
        let tag = self.sign(share, expiry);
        UnlockTicket(format!("{expiry}.{}", URL_SAFE_NO_PAD.encode(tag)))
    }

    /// Returns whether `ticket` is a valid, unexpired proof for `share`.
    #[must_use]
    pub fn verify(&self, ticket: &str, share: ShareLinkId, now: DateTime<Utc>) -> bool {
        // Bounded before parsing so a hostile cookie cannot make this expensive.
        if ticket.len() > 128 {
            return false;
        }
        let Some((expiry, tag)) = ticket.split_once('.') else {
            return false;
        };
        let Ok(expiry) = expiry.parse::<i64>() else {
            return false;
        };
        if expiry <= now.timestamp() {
            return false;
        }
        let Ok(presented) = URL_SAFE_NO_PAD.decode(tag) else {
            return false;
        };
        let expected = self.sign(share, expiry);
        bool::from(presented.ct_eq(&expected))
    }

    fn sign(&self, share: ShareLinkId, expiry: i64) -> [u8; 32] {
        let mut mac =
            HmacSha256::new_from_slice(self.key.as_slice()).expect("HMAC accepts a 32-byte key");
        mac.update(share.as_uuid().as_bytes());
        mac.update(b".");
        mac.update(expiry.to_string().as_bytes());
        mac.finalize().into_bytes().into()
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;

    fn issuer() -> TicketIssuer {
        TicketIssuer::derive(b"a-test-master-key-of-sufficient-length").expect("issuer")
    }

    #[test]
    fn a_ticket_proves_exactly_one_share_until_it_expires() {
        let issuer = issuer();
        let share = ShareLinkId::new();
        let now = Utc::now();
        let ticket = issuer.issue(share, now + Duration::hours(1));
        assert!(issuer.verify(ticket.as_str(), share, now));
        assert!(!issuer.verify(ticket.as_str(), ShareLinkId::new(), now));
        assert!(!issuer.verify(ticket.as_str(), share, now + Duration::hours(2)));
    }

    #[test]
    fn a_forged_or_tampered_ticket_is_rejected() {
        let issuer = issuer();
        let share = ShareLinkId::new();
        let now = Utc::now();
        let ticket = issuer.issue(share, now + Duration::hours(1)).into_string();
        let (expiry, tag) = ticket.split_once('.').expect("ticket shape");

        // Extending the expiry invalidates the signature it is part of.
        let extended = format!("{}.{tag}", expiry.parse::<i64>().expect("expiry") + 86_400);
        assert!(!issuer.verify(&extended, share, now));

        for candidate in ["", ".", "notanumber.abc", "9999999999.notbase64!!"] {
            assert!(!issuer.verify(candidate, share, now));
        }
        assert!(!issuer.verify(&"a".repeat(200), share, now));
    }

    #[test]
    fn tickets_from_a_different_deployment_key_are_worthless() {
        let share = ShareLinkId::new();
        let now = Utc::now();
        let ticket = issuer().issue(share, now + Duration::hours(1));
        let other =
            TicketIssuer::derive(b"an-entirely-different-master-key-value").expect("issuer");
        assert!(!other.verify(ticket.as_str(), share, now));
    }

    #[test]
    fn the_signing_key_never_renders_itself() {
        assert_eq!(format!("{:?}", issuer()), "TicketIssuer(<redacted>)");
    }
}
