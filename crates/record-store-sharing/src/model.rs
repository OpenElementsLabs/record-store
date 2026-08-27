//! Durable descriptors for external object-access capabilities.
//!
//! Shares and embeds are modelled as two distinct capabilities rather than one
//! with a flag. They differ in who holds them (a person versus a website), in
//! what they may carry (a password and an access budget versus an origin
//! allowlist and caching), and in how they are delivered (a Record Store page versus raw
//! bytes). Collapsing them would force every one of those differences to become
//! a conditional, and the first time one of the conditionals was forgotten the
//! result would be a security decision applied to the wrong capability.

use chrono::{DateTime, Utc};
use record_store_core::{BucketId, BucketName, EmbedLinkId, ObjectKey, ShareLinkId, VersionId};
use serde::{Deserialize, Serialize};

use crate::{origin::AllowedOrigin, password::PasswordHash};

/// Which object version a capability resolves to when it is used.
///
/// This is never inferred. A logo that should track edits and a signed contract
/// that must never change are both legitimate, and the difference between them
/// cannot be recovered later from a capability that did not record it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum VersionMode {
    /// Resolve to whichever version is current at the moment of access.
    FollowCurrent,
    /// Resolve to exactly this immutable version, forever.
    Pinned {
        /// The pinned immutable version.
        version_id: VersionId,
    },
}

impl VersionMode {
    /// Returns the pinned version, if this capability pins one.
    #[must_use]
    pub const fn pinned(self) -> Option<VersionId> {
        match self {
            Self::FollowCurrent => None,
            Self::Pinned { version_id } => Some(version_id),
        }
    }

    /// A stable low-cardinality label safe for metrics and audit metadata.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::FollowCurrent => "current",
            Self::Pinned { .. } => "pinned",
        }
    }
}

/// The logical object a capability points at.
///
/// The bucket identifier is stored alongside the name so a capability keeps
/// working through a rename and stops working if the bucket is deleted and
/// recreated under the same name. Nothing physical — placement, replicas,
/// shards, nodes — appears here, because a capability addresses the logical
/// object service and never a copy of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityTarget {
    /// Stable owning bucket.
    pub bucket_id: BucketId,
    /// Bucket name at creation time, used for service lookups.
    pub bucket: BucketName,
    /// Logical object key.
    pub key: ObjectKey,
    /// Which version this capability resolves to.
    pub version: VersionMode,
}

impl CapabilityTarget {
    /// Returns the file name a recipient should see, without the key's path.
    #[must_use]
    pub fn file_name(&self) -> &str {
        self.key.as_str().rsplit('/').next().unwrap_or("download")
    }

    /// Returns the canonical audit resource for this target.
    ///
    /// Deliberately the same `bucket:name/key` shape the policy engine uses, so
    /// share activity is greppable alongside every other object operation.
    #[must_use]
    pub fn audit_resource(&self) -> String {
        format!("bucket:{}/{}", self.bucket, self.key)
    }
}

/// What a share link's holder may do with the object.
///
/// Deliberately three values rather than a policy language. A share is a
/// narrowly scoped external capability; expressing it in the same vocabulary as
/// bucket administration would invite it to grow the same authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharePermission {
    /// Inline viewing only. No attachment response is offered.
    View,
    /// Download only. Nothing is rendered inline.
    Download,
    /// Both inline viewing and download.
    ViewAndDownload,
}

impl SharePermission {
    /// Whether the recipient may view the object inline.
    #[must_use]
    pub const fn allows_view(self) -> bool {
        matches!(self, Self::View | Self::ViewAndDownload)
    }

    /// Whether the recipient may download the object as an attachment.
    #[must_use]
    pub const fn allows_download(self) -> bool {
        matches!(self, Self::Download | Self::ViewAndDownload)
    }

    /// A stable low-cardinality label safe for metrics and audit metadata.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::View => "view",
            Self::Download => "download",
            Self::ViewAndDownload => "view_and_download",
        }
    }
}

/// How embedded bytes are presented to the requesting browser.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbedDisposition {
    /// Rendered in place by an `<img>`, `<video>`, or `<audio>` element.
    #[default]
    Inline,
    /// Offered as a download even when reached from a page.
    Attachment,
}

impl EmbedDisposition {
    /// A stable low-cardinality label safe for metrics and audit metadata.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::Attachment => "attachment",
        }
    }
}

/// The lifecycle state of a capability, as an operator sees it.
///
/// Ordered by how much attention each state deserves, and carried as a value so
/// the console never has to re-derive it from three nullable timestamps and get
/// the precedence wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityStatus {
    /// Usable right now.
    Active,
    /// Deliberately withdrawn. This is terminal.
    Revoked,
    /// Past its expiry.
    Expired,
    /// Its access budget is used up.
    Exhausted,
}

impl CapabilityStatus {
    /// Whether a capability in this state may still authorize access.
    #[must_use]
    pub const fn usable(self) -> bool {
        matches!(self, Self::Active)
    }

    /// A stable low-cardinality label safe for metrics and audit metadata.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Revoked => "revoked",
            Self::Expired => "expired",
            Self::Exhausted => "exhausted",
        }
    }
}

/// A durable, revocable capability intended for a person.
///
/// The token itself is not a field: it is held only as a lookup digest and an
/// encrypted copy, both of which live in the store rather than in this
/// descriptor. That keeps every listing, serialization, and log statement in the
/// codebase structurally incapable of leaking it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareLink {
    /// Stable non-secret identifier, safe for audit records and URLs in the
    /// management plane.
    pub id: ShareLinkId,
    /// Operator-facing name, so a list of shares is readable.
    pub label: String,
    /// The logical object and version this share resolves to.
    pub target: CapabilityTarget,
    /// The management identity that created it. Never a credential.
    pub created_by: String,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Expiry, when one was set.
    pub expires_at: Option<DateTime<Utc>>,
    /// What the recipient may do.
    pub permission: SharePermission,
    /// Verifier for an optional share password. Never the password.
    #[serde(default)]
    pub password: Option<PasswordHash>,
    /// Optional strict ceiling on successful authorizations.
    #[serde(default)]
    pub maximum_access_count: Option<u32>,
    /// Successful authorizations so far.
    #[serde(default)]
    pub access_count: u32,
    /// When the share was withdrawn, if it was.
    #[serde(default)]
    pub revoked_at: Option<DateTime<Utc>>,
    /// Last successful authorization.
    #[serde(default)]
    pub last_accessed_at: Option<DateTime<Utc>>,
}

impl ShareLink {
    /// Returns the share's state at `now`.
    ///
    /// Revocation is checked first and unconditionally: an operator who revokes
    /// a share has made a decision that no other field may soften.
    #[must_use]
    pub fn status(&self, now: DateTime<Utc>) -> CapabilityStatus {
        if self.revoked_at.is_some() {
            return CapabilityStatus::Revoked;
        }
        if self.expires_at.is_some_and(|expiry| expiry <= now) {
            return CapabilityStatus::Expired;
        }
        if self
            .maximum_access_count
            .is_some_and(|maximum| self.access_count >= maximum)
        {
            return CapabilityStatus::Exhausted;
        }
        CapabilityStatus::Active
    }

    /// Whether the share is protected by a password.
    #[must_use]
    pub const fn password_protected(&self) -> bool {
        self.password.is_some()
    }
}

/// A durable, revocable capability intended for an application or website.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbedLink {
    /// Stable non-secret identifier.
    pub id: EmbedLinkId,
    /// Operator-facing name.
    pub label: String,
    /// The logical object and version this embed resolves to.
    pub target: CapabilityTarget,
    /// The management identity that created it.
    pub created_by: String,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Expiry, when one was set.
    pub expires_at: Option<DateTime<Utc>>,
    /// Origins permitted to read these bytes from a browser. Empty means the
    /// embed is not origin-restricted, and the unguessable token is the whole
    /// capability.
    #[serde(default)]
    pub allowed_origins: Vec<AllowedOrigin>,
    /// How the bytes are presented.
    #[serde(default)]
    pub disposition: EmbedDisposition,
    /// Media type recorded at creation, used to reject an embed whose object
    /// later becomes something that must not be served inline.
    pub content_type: String,
    /// When the embed was withdrawn, if it was.
    #[serde(default)]
    pub revoked_at: Option<DateTime<Utc>>,
    /// Last successful authorization.
    #[serde(default)]
    pub last_accessed_at: Option<DateTime<Utc>>,
    /// Successful authorizations so far. Operational telemetry, not a limit.
    #[serde(default)]
    pub access_count: u64,
    /// Last administrative update.
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
}

impl EmbedLink {
    /// Returns the embed's state at `now`.
    #[must_use]
    pub fn status(&self, now: DateTime<Utc>) -> CapabilityStatus {
        if self.revoked_at.is_some() {
            return CapabilityStatus::Revoked;
        }
        if self.expires_at.is_some_and(|expiry| expiry <= now) {
            return CapabilityStatus::Expired;
        }
        CapabilityStatus::Active
    }

    /// Whether the embed restricts which origins may read it.
    #[must_use]
    pub fn origin_restricted(&self) -> bool {
        !self.allowed_origins.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use chrono::Duration;

    use super::*;

    fn target() -> CapabilityTarget {
        CapabilityTarget {
            bucket_id: BucketId::new(),
            bucket: BucketName::from_str("reports").expect("bucket name"),
            key: ObjectKey::new("2026/q1/summary.pdf").expect("object key"),
            version: VersionMode::FollowCurrent,
        }
    }

    fn share() -> ShareLink {
        ShareLink {
            id: ShareLinkId::new(),
            label: "Board review".to_owned(),
            target: target(),
            created_by: "management:system-administrator".to_owned(),
            created_at: Utc::now(),
            expires_at: None,
            permission: SharePermission::ViewAndDownload,
            password: None,
            maximum_access_count: None,
            access_count: 0,
            revoked_at: None,
            last_accessed_at: None,
        }
    }

    #[test]
    fn revocation_outranks_every_other_state() {
        let now = Utc::now();
        let mut link = share();
        link.expires_at = Some(now - Duration::hours(1));
        link.maximum_access_count = Some(1);
        link.access_count = 5;
        link.revoked_at = Some(now - Duration::minutes(1));
        assert_eq!(link.status(now), CapabilityStatus::Revoked);
        assert!(!link.status(now).usable());
    }

    #[test]
    fn expiry_is_inclusive_so_a_share_is_dead_at_its_expiry_instant() {
        let now = Utc::now();
        let mut link = share();
        link.expires_at = Some(now);
        assert_eq!(link.status(now), CapabilityStatus::Expired);
        link.expires_at = Some(now + Duration::seconds(1));
        assert_eq!(link.status(now), CapabilityStatus::Active);
    }

    #[test]
    fn an_exhausted_budget_is_reported_distinctly_from_expiry() {
        let now = Utc::now();
        let mut link = share();
        link.maximum_access_count = Some(3);
        link.access_count = 3;
        assert_eq!(link.status(now), CapabilityStatus::Exhausted);
    }

    #[test]
    fn permissions_expose_exactly_the_two_capabilities_they_name() {
        assert!(SharePermission::View.allows_view());
        assert!(!SharePermission::View.allows_download());
        assert!(!SharePermission::Download.allows_view());
        assert!(SharePermission::Download.allows_download());
        assert!(SharePermission::ViewAndDownload.allows_view());
        assert!(SharePermission::ViewAndDownload.allows_download());
    }

    #[test]
    fn a_target_reports_the_recipient_facing_file_name_without_its_prefix() {
        assert_eq!(target().file_name(), "summary.pdf");
    }
}
