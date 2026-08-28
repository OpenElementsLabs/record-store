use std::fmt::Debug;

use thiserror::Error;

/// Stable authentication failure categories.
#[derive(Debug, Error)]
pub enum AuthenticationError {
    /// No valid credential matched the proof.
    #[error("invalid credentials")]
    InvalidCredentials,
    /// The matching credential cannot currently be used.
    #[error("credential is disabled or expired")]
    CredentialInactive,
    /// The credential backend failed internally.
    #[error("authentication backend unavailable")]
    BackendUnavailable,
}

/// Stable authorization failure categories.
#[derive(Debug, Error)]
pub enum AuthorizationError {
    /// The principal lacks the required permission.
    #[error("permission denied")]
    Denied,
    /// The policy backend failed internally.
    #[error("authorization backend unavailable")]
    BackendUnavailable,
}

/// Credential lookup failures intentionally safe for protocol mapping.
#[derive(Debug, Error)]
pub enum CredentialLookupError {
    /// The public access key is unknown.
    #[error("access key was not found")]
    UnknownAccessKey,
    /// The credential or account is inactive.
    #[error("credential is inactive")]
    Inactive,
    /// Durable credential state could not be read or decrypted.
    #[error("credential backend unavailable")]
    Backend,
}

/// Durable credential-management failures for administrator-facing operations.
#[derive(Debug, Error)]
pub enum CredentialStoreError {
    /// Credential directory creation failed.
    #[error("failed to prepare credential directory: {0}")]
    Directory(#[source] std::io::Error),
    /// Encrypted durable credentials require an explicitly supplied stable key.
    #[error("an explicit credential master key is required")]
    MasterKeyRequired,
    /// A generated access key collided and issuance can be retried.
    #[error("generated access key collided")]
    AccessKeyCollision,
    /// Requested service account is absent.
    #[error("service account was not found")]
    AccountNotFound,
    /// Requested credential is absent.
    #[error("credential was not found")]
    CredentialNotFound,
    /// Policy name already exists.
    #[error("policy already exists")]
    PolicyAlreadyExists,
    /// Requested policy is absent.
    #[error("policy was not found")]
    PolicyNotFound,
    /// Credential is disabled or expired.
    #[error("credential is inactive")]
    CredentialInactive,
    /// Credential index points to missing state.
    #[error("credential index is inconsistent")]
    CorruptIndex,
    /// Invalid administrative input.
    #[error("invalid service account: {0}")]
    InvalidInput(String),
    /// Credential encoding failed.
    #[error("credential encoding failed: {0}")]
    Encoding(#[from] serde_json::Error),
    /// Cryptographic encryption, decryption, or derivation failed.
    #[error("credential cryptography failed")]
    Cryptography,
    /// Embedded credential database failed.
    #[error("credential database failed: {0}")]
    Database(String),
    /// Blocking credential task failed.
    #[error("credential task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}
