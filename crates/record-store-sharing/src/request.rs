use chrono::{DateTime, Utc};
use record_store_core::{BucketId, BucketName, ObjectKey, ShareLinkId};

use crate::*;

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
