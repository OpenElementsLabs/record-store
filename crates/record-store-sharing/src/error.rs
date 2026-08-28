use thiserror::Error;

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
