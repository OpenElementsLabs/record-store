//! Secure, revocable external access to stored objects.
//!
//! Record Store objects are reachable three ways once this crate is wired in. An
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

mod error;
mod policy;
mod request;
mod service;
mod support;

#[cfg(test)]
mod tests;

pub use error::SharingError;
pub use policy::SharingPolicy;
pub use request::{
    AccessDenial, CreateEmbedRequest, CreateShareRequest, IssuedCapability, ShareLookup,
    UnlockFailure,
};
pub use service::SharingService;
pub use support::{MAXIMUM_LABEL_LENGTH, SharedSharingService, pinned_version};

