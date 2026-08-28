use async_trait::async_trait;

use crate::*;

/// Authentication boundary for future credential backends.
#[async_trait]
pub trait Authenticator: Send + Sync {
    /// Verifies opaque credential proof and returns an authenticated principal.
    /// Implementations must use constant-time comparison where applicable.
    async fn authenticate(
        &self,
        public_key_id: &str,
        credential_proof: &[u8],
    ) -> Result<Principal, AuthenticationError>;
}

/// Authorization boundary kept separate from credential verification.
#[async_trait]
pub trait Authorizer: Send + Sync {
    /// Returns success only when the requested permission is granted.
    async fn authorize(&self, context: AuthorizationContext<'_>) -> Result<(), AuthorizationError>;
}

/// S3 signing-credential lookup boundary.
#[async_trait]
pub trait SigningCredentialProvider: Send + Sync {
    /// Resolves active signing material without exposing persistence details.
    async fn signing_secret(
        &self,
        access_key_id: &str,
    ) -> Result<(Principal, SigningSecret), CredentialLookupError>;
}
