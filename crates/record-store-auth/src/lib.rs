//! Authentication and authorization contracts.
//!
//! This crate intentionally provides boundaries and safe persisted descriptors,
//! not an IAM implementation. Secret verifiers belong in credential backends and
//! must never be represented by [`Credential`].

mod accounts;
mod boundary;
mod crypto;
mod error;
mod evaluate;
mod keys;
mod manager;
mod model;
mod policies;
mod schema;
mod secret;
mod validation;

#[cfg(test)]
mod tests;

pub use boundary::{Authenticator, Authorizer, SigningCredentialProvider};
pub use error::{
    AuthenticationError, AuthorizationError, CredentialLookupError, CredentialStoreError,
};
pub use manager::{CredentialManager, IssuedServiceAccount, ServiceAccountInfo};
pub use model::{
    Action, AuthorizationContext, Credential, Permission, Policy, PolicyEffect, PolicyStatement,
    Principal, ServiceAccount,
};
pub use secret::SigningSecret;
