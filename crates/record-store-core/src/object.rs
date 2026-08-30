use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::*;

/// Logical bucket metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bucket {
    /// Stable bucket identifier.
    pub id: BucketId,
    /// Owning organization.
    pub organization_id: OrganizationId,
    /// User-facing bucket name.
    pub name: BucketName,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Current object-versioning behavior.
    #[serde(default)]
    pub versioning: VersioningState,
    /// Limits enforced transactionally at metadata publication.
    #[serde(default)]
    pub quota: BucketQuota,
    /// Storage class new objects in this bucket are placed on.
    ///
    /// `None` means the deployment default class, which is what every bucket
    /// created before storage classes existed resolves to. Changing this affects
    /// where new objects are placed; it never moves objects already written.
    #[serde(default)]
    pub storage_class: Option<StorageClass>,
    /// Optional physical durability policy for new object versions.
    ///
    /// `None` means the deployment default: single-copy in standalone mode and
    /// replication in cluster mode. Changing this never changes old versions.
    #[serde(default)]
    pub durability_policy: Option<DurabilityProfile>,
    /// Which web origins may reach this bucket's objects from a browser.
    ///
    /// `None` means none of them, which is the only safe default: a bucket
    /// nobody configured must not be readable by any page on the internet. It
    /// lives on the bucket rather than in deployment configuration so that one
    /// permissive application cannot set the policy for every other bucket.
    #[serde(default)]
    pub cors: Option<CorsConfiguration>,
}

/// Persisted object metadata, independent of the physical object layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectMetadata {
    /// Immutable physical object identifier.
    pub id: ObjectId,
    /// Logical owning bucket.
    pub bucket_id: BucketId,
    /// Logical object key.
    pub key: ObjectKey,
    /// Current version identifier.
    pub version_id: VersionId,
    /// Payload length in bytes.
    pub size: u64,
    /// Payload integrity checksum.
    pub checksum: Checksum,
    /// Versioned on-disk representation. Cryptographic key material is never
    /// part of this public metadata structure.
    #[serde(default)]
    pub payload_format: PayloadFormat,
    /// Physical durability actually used for this immutable version.
    ///
    /// This is persisted per version and is never inferred from the bucket's
    /// current policy. The default reads pre-durability standalone metadata.
    #[serde(default)]
    pub durability: DurabilityProfile,
    /// Stable protocol-facing entity tag.
    pub etag: ETag,
    /// Optional media type supplied by the caller.
    pub content_type: Option<String>,
    /// Caller-supplied metadata with deterministic serialization order.
    pub custom_metadata: BTreeMap<String, String>,
    /// Logical creation time.
    pub created_at: DateTime<Utc>,
    /// Last modification time.
    pub modified_at: DateTime<Utc>,
}

/// Versioned physical representation used for an immutable payload.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PayloadFormat {
    /// Historical and explicitly unencrypted payload bytes.
    #[default]
    Plaintext,
    /// Record Store envelope encryption format 1 using chunked AES-256-GCM.
    Aes256GcmEnvelopeV1,
}

/// A concise reference to an immutable object version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectVersion {
    /// Version identifier.
    pub id: VersionId,
    /// Physical payload identifier.
    pub object_id: ObjectId,
    /// Time at which the version was committed.
    pub created_at: DateTime<Utc>,
}

/// A logical delete marker in a version-enabled bucket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteMarker {
    /// Stable public version identifier.
    pub version_id: VersionId,
    /// Owning bucket.
    pub bucket_id: BucketId,
    /// Deleted logical key.
    pub key: ObjectKey,
    /// Time the marker became current.
    pub created_at: DateTime<Utc>,
}

/// One immutable entry in a key's version history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObjectVersionRecord {
    /// A version with an immutable physical payload.
    Object {
        /// Immutable payload metadata.
        metadata: ObjectMetadata,
        /// Whether S3 exposes this as the special `null` version.
        is_null: bool,
    },
    /// A logical deletion without a payload.
    DeleteMarker {
        /// Immutable logical deletion metadata.
        marker: DeleteMarker,
        /// Whether S3 exposes this as the special `null` version.
        is_null: bool,
    },
}

impl ObjectVersionRecord {
    /// Returns the stable version identifier.
    #[must_use]
    pub const fn version_id(&self) -> VersionId {
        match self {
            Self::Object { metadata, .. } => metadata.version_id,
            Self::DeleteMarker { marker, .. } => marker.version_id,
        }
    }

    /// Returns the logical key.
    #[must_use]
    pub fn key(&self) -> &ObjectKey {
        match self {
            Self::Object { metadata, .. } => &metadata.key,
            Self::DeleteMarker { marker, .. } => &marker.key,
        }
    }

    /// Returns the creation timestamp used for deterministic ordering.
    #[must_use]
    pub const fn created_at(&self) -> DateTime<Utc> {
        match self {
            Self::Object { metadata, .. } => metadata.created_at,
            Self::DeleteMarker { marker, .. } => marker.created_at,
        }
    }

    /// Returns true for a logical deletion entry.
    #[must_use]
    pub const fn is_delete_marker(&self) -> bool {
        matches!(self, Self::DeleteMarker { .. })
    }

    /// Returns whether this entry is the special unversioned/null entry.
    #[must_use]
    pub const fn is_null(&self) -> bool {
        match self {
            Self::Object { is_null, .. } | Self::DeleteMarker { is_null, .. } => *is_null,
        }
    }
}

/// Lifecycle state of a durable multipart upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MultipartUploadState {
    /// Parts may be added or replaced.
    Active,
    /// A final immutable payload has been allocated and completion is recoverable.
    Completing { object_id: ObjectId },
}

/// Durable multipart upload descriptor. It contains no caller-controlled paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultipartUpload {
    /// Opaque client-facing upload identifier.
    pub id: UploadId,
    /// Destination bucket.
    pub bucket_id: BucketId,
    /// Destination logical key.
    pub key: ObjectKey,
    /// Media type selected at initiation.
    pub content_type: Option<String>,
    /// Custom metadata selected at initiation.
    pub custom_metadata: BTreeMap<String, String>,
    /// Creation timestamp.
    pub initiated_at: DateTime<Utc>,
    /// Crash-recovery state.
    pub state: MultipartUploadState,
}

/// One durably persisted multipart payload part.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadedPart {
    /// Owning upload.
    pub upload_id: UploadId,
    /// Validated one-based part number.
    pub number: PartNumber,
    /// Record Store-controlled immutable payload identifier.
    pub object_id: ObjectId,
    /// Part size in bytes.
    pub size: u64,
    /// Strong internal checksum.
    pub checksum: Checksum,
    /// Versioned physical representation of the durable part payload.
    #[serde(default)]
    pub payload_format: PayloadFormat,
    /// S3-compatible part ETag.
    pub etag: ETag,
    /// Last successful upload time.
    pub modified_at: DateTime<Utc>,
}

/// A caller completion entry, validated against durable parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedPart {
    /// Part number in ascending manifest order.
    pub number: PartNumber,
    /// ETag supplied by the completion request.
    pub etag: ETag,
}
