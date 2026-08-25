//! Opaque capability tokens.
//!
//! A capability token *is* the authority: whoever holds one can read the object
//! it names, so it is treated with the same care as a credential. It is drawn
//! from the operating system's cryptographic generator, never derived from
//! anything an observer could predict, never used as a database key in the
//! clear, and never rendered by [`Debug`].

use std::fmt::{self, Debug, Formatter};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::SharingError;

/// Bytes of entropy behind every capability token.
///
/// 256 bits. The tokens are guessed at over the public internet by anyone who
/// finds the route, and unlike a password there is no account to lock, so the
/// only defence that scales is making the search space unreachable.
pub const TOKEN_ENTROPY_BYTES: usize = 32;

/// Length of a token's canonical text form.
pub const TOKEN_TEXT_LENGTH: usize = 43;

/// A secret capability token.
///
/// Wrapped rather than passed as a `String` so that it cannot be logged by
/// accident, formatted into an error, or serialized into a response: every one
/// of those requires calling [`CapabilityToken::expose`], which is greppable.
#[derive(Clone)]
pub struct CapabilityToken(Zeroizing<String>);

impl CapabilityToken {
    /// Draws a fresh token from the operating system's cryptographic generator.
    pub fn generate() -> Result<Self, SharingError> {
        let mut bytes = Zeroizing::new([0_u8; TOKEN_ENTROPY_BYTES]);
        getrandom::fill(bytes.as_mut_slice()).map_err(|_| SharingError::EntropyUnavailable)?;
        Ok(Self(Zeroizing::new(
            URL_SAFE_NO_PAD.encode(bytes.as_slice()),
        )))
    }

    /// Accepts a token presented by a caller, rejecting anything malformed.
    ///
    /// Shape is validated before the store is consulted so that a probe with a
    /// hostile path segment never reaches a lookup, and so that the cost of an
    /// obviously invalid request stays at a length check.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        if value.len() != TOKEN_TEXT_LENGTH {
            return None;
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            return None;
        }
        Some(Self(Zeroizing::new(value.to_owned())))
    }

    /// Returns the token text. Every call site is a deliberate disclosure.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Returns the lookup digest stored in place of the token.
    #[must_use]
    pub fn digest(&self) -> TokenDigest {
        TokenDigest(Sha256::digest(self.0.as_bytes()).into())
    }
}

impl Debug for CapabilityToken {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str("CapabilityToken(<redacted>)")
    }
}

impl PartialEq for CapabilityToken {
    fn eq(&self, other: &Self) -> bool {
        bool::from(self.0.as_bytes().ct_eq(other.0.as_bytes()))
    }
}

impl Eq for CapabilityToken {}

/// The stored stand-in for a capability token.
///
/// A plain SHA-256 rather than a password hash, and deliberately so: the input
/// is 256 uniformly random bits, which no amount of iteration would make harder
/// to brute-force than it already is. Password stretching exists to compensate
/// for low-entropy human input, and applying it here would buy nothing while
/// making every public request pay for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TokenDigest([u8; 32]);

impl TokenDigest {
    /// Returns the digest bytes, used as the store's lookup key.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Redacts capability tokens out of a request path before it is logged.
///
/// The tracing stack records request paths verbatim, and a public capability
/// route carries its secret in the path. Without this, ordinary operational
/// logging would become a durable list of working share links.
#[must_use]
pub fn redact_capability_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut segments = path.split('/');
    // A path always begins with the empty segment before the leading slash.
    let mut redact_next = false;
    let mut first = true;
    for segment in segments.by_ref() {
        if !first {
            out.push('/');
        }
        first = false;
        if redact_next && !segment.is_empty() {
            out.push_str("<redacted>");
            redact_next = false;
            continue;
        }
        redact_next = matches!(segment, "s" | "e");
        out.push_str(segment);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn generated_tokens_are_unique_and_canonically_shaped() {
        let mut seen = HashSet::new();
        for _ in 0..256 {
            let token = CapabilityToken::generate().expect("token");
            assert_eq!(token.expose().len(), TOKEN_TEXT_LENGTH);
            assert!(CapabilityToken::parse(token.expose()).is_some());
            assert!(seen.insert(token.expose().to_owned()), "token repeated");
        }
    }

    #[test]
    fn generated_tokens_carry_the_advertised_entropy() {
        // A base64url token of this length decodes to exactly the promised
        // number of random bytes; a shorter alphabet or a truncated draw would
        // show up here rather than as a quietly weaker capability.
        let token = CapabilityToken::generate().expect("token");
        let decoded = URL_SAFE_NO_PAD.decode(token.expose()).expect("base64url");
        assert_eq!(decoded.len(), TOKEN_ENTROPY_BYTES);
        assert_ne!(decoded, vec![0_u8; TOKEN_ENTROPY_BYTES]);
    }

    #[test]
    fn malformed_tokens_are_rejected_before_any_lookup() {
        for candidate in [
            "",
            "short",
            &"a".repeat(TOKEN_TEXT_LENGTH - 1),
            &"a".repeat(TOKEN_TEXT_LENGTH + 1),
            &format!("{}/", "a".repeat(TOKEN_TEXT_LENGTH - 1)),
            &format!("{}+", "a".repeat(TOKEN_TEXT_LENGTH - 1)),
            &format!("{}.", "a".repeat(TOKEN_TEXT_LENGTH - 1)),
        ] {
            assert!(
                CapabilityToken::parse(candidate).is_none(),
                "accepted malformed token: {candidate}"
            );
        }
    }

    #[test]
    fn a_token_never_renders_itself_in_debug_output() {
        let token = CapabilityToken::generate().expect("token");
        let rendered = format!("{token:?}");
        assert_eq!(rendered, "CapabilityToken(<redacted>)");
        assert!(!rendered.contains(token.expose()));
    }

    #[test]
    fn digests_differ_for_different_tokens_and_repeat_for_the_same_one() {
        let first = CapabilityToken::generate().expect("token");
        let second = CapabilityToken::generate().expect("token");
        assert_eq!(first.digest(), first.digest());
        assert_ne!(first.digest(), second.digest());
    }

    #[test]
    fn capability_paths_are_redacted_before_they_reach_a_log() {
        let token = CapabilityToken::generate().expect("token");
        let path = format!("/s/{}", token.expose());
        let redacted = redact_capability_path(&path);
        assert_eq!(redacted, "/s/<redacted>");
        assert!(!redacted.contains(token.expose()));

        assert_eq!(
            redact_capability_path(&format!("/e/{}/thumb", token.expose())),
            "/e/<redacted>/thumb"
        );
        // Paths that merely start with the same letters are left alone.
        assert_eq!(
            redact_capability_path("/api/v1/buckets/reports"),
            "/api/v1/buckets/reports"
        );
        assert_eq!(redact_capability_path("/s"), "/s");
        assert_eq!(redact_capability_path("/"), "/");
    }
}
