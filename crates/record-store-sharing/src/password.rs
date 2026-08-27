//! Optional share passwords.
//!
//! A share password is a human-chosen secret, so it is stored the way human
//! secrets have to be stored: through a memory-hard password hash with a random
//! salt, never a bare digest. The verifier is compared in constant time by the
//! underlying implementation, and neither the password nor the verifier is ever
//! written to a log, an audit record, or a response.

use argon2::{
    Argon2,
    password_hash::{PasswordHash as PhcHash, PasswordHasher, PasswordVerifier, SaltString},
};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::SharingError;

/// Smallest password Record Store will accept on a share.
///
/// Short enough not to be an obstacle for a link sent alongside its password,
/// long enough that the rate limiter is defending a search space rather than a
/// four-digit one.
pub const MINIMUM_PASSWORD_LENGTH: usize = 8;

/// Largest password accepted, so that hashing cost stays bounded by policy
/// rather than by whatever a caller chooses to send.
pub const MAXIMUM_PASSWORD_LENGTH: usize = 256;

/// A stored password verifier in PHC string format.
///
/// The format records the algorithm and its parameters alongside the salt and
/// the tag, so a future parameter increase can verify old hashes while writing
/// new ones — without a migration and without a flag day.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PasswordHash(String);

impl PasswordHash {
    /// Hashes a share password for storage.
    pub fn create(password: &str) -> Result<Self, SharingError> {
        validate(password)?;
        let mut salt_bytes = [0_u8; 16];
        getrandom::fill(&mut salt_bytes).map_err(|_| SharingError::EntropyUnavailable)?;
        let salt =
            SaltString::encode_b64(&salt_bytes).map_err(|_| SharingError::PasswordHashFailed)?;
        let encoded = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map_err(|_| SharingError::PasswordHashFailed)?
            .to_string();
        Ok(Self(encoded))
    }

    /// Returns whether `candidate` is the password behind this verifier.
    ///
    /// A malformed stored verifier is treated as a failed comparison rather
    /// than an error: the caller's decision is the same either way, and a
    /// distinguishable error would tell an attacker something about the record.
    #[must_use]
    pub fn verify(&self, candidate: &str) -> bool {
        if candidate.len() > MAXIMUM_PASSWORD_LENGTH {
            return false;
        }
        let candidate = Zeroizing::new(candidate.to_owned());
        let Ok(parsed) = PhcHash::new(&self.0) else {
            return false;
        };
        Argon2::default()
            .verify_password(candidate.as_bytes(), &parsed)
            .is_ok()
    }
}

impl std::fmt::Debug for PasswordHash {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PasswordHash(<redacted>)")
    }
}

fn validate(password: &str) -> Result<(), SharingError> {
    if password.chars().count() < MINIMUM_PASSWORD_LENGTH {
        return Err(SharingError::InvalidPassword(format!(
            "a share password must be at least {MINIMUM_PASSWORD_LENGTH} characters"
        )));
    }
    if password.len() > MAXIMUM_PASSWORD_LENGTH {
        return Err(SharingError::InvalidPassword(format!(
            "a share password must be at most {MAXIMUM_PASSWORD_LENGTH} bytes"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_verifies_only_against_itself() {
        let hash = PasswordHash::create("correct horse battery").expect("hash");
        assert!(hash.verify("correct horse battery"));
        assert!(!hash.verify("Correct horse battery"));
        assert!(!hash.verify("correct horse batter"));
        assert!(!hash.verify(""));
    }

    #[test]
    fn the_stored_form_is_a_salted_memory_hard_hash_not_a_digest() {
        let first = PasswordHash::create("correct horse battery").expect("hash");
        let second = PasswordHash::create("correct horse battery").expect("hash");
        // Distinct salts mean the same password never produces the same record,
        // which is precisely what a bare SHA-256 would fail to do.
        assert_ne!(first, second);
        assert!(first.0.starts_with("$argon2"));
        assert!(!first.0.contains("correct horse battery"));
    }

    #[test]
    fn short_and_oversized_passwords_are_refused_at_creation() {
        assert!(PasswordHash::create("short").is_err());
        assert!(PasswordHash::create(&"a".repeat(MAXIMUM_PASSWORD_LENGTH + 1)).is_err());
        assert!(PasswordHash::create(&"a".repeat(MINIMUM_PASSWORD_LENGTH)).is_ok());
    }

    #[test]
    fn an_oversized_candidate_is_rejected_without_hashing_it() {
        let hash = PasswordHash::create("correct horse battery").expect("hash");
        assert!(!hash.verify(&"a".repeat(MAXIMUM_PASSWORD_LENGTH + 1)));
    }

    #[test]
    fn a_verifier_never_renders_itself_in_debug_output() {
        let hash = PasswordHash::create("correct horse battery").expect("hash");
        assert_eq!(format!("{hash:?}"), "PasswordHash(<redacted>)");
    }

    #[test]
    fn a_corrupt_stored_verifier_fails_closed() {
        let hash = PasswordHash("not a phc string".to_owned());
        assert!(!hash.verify("not a phc string"));
    }
}
