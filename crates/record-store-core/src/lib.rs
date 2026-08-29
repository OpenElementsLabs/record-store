//! Fundamental domain types shared by Record Store components.

mod cors;

pub use crate::cors::{
    CorsConfiguration, CorsGrant, CorsMethod, CorsPattern, CorsRule, MAXIMUM_CORS_MAX_AGE_SECONDS,
    MAXIMUM_CORS_RULES, parse_requested_headers,
};

mod checksum;
mod durability;
mod error;
mod ids;
mod lifecycle;
mod names;
mod object;
mod preview;
mod quota;
mod range;
mod shard;

pub use checksum::{Checksum, ChecksumAlgorithm, ETag};
pub use durability::{DurabilityProfile, ErasureProfile, ReplicationProfile};
pub use error::{CoreError, ErrorCategory};
pub use ids::{
    AuditEventId, BucketId, ClusterId, ClusterOperationId, CredentialId, DeviceId, EmbedLinkId,
    EventId, JoinTokenId, LifecycleRuleId, NodeCredentialId, NodeId, ObjectId, OrganizationId,
    PolicyId, ReplicaTaskId, ServiceAccountId, ShardId, ShareLinkId, StripeId, UploadId, VersionId,
    WebhookId,
};
pub use lifecycle::{LifecycleRule, ObjectRetention, RetentionMode, StorageUsage};
pub use names::{BucketName, ObjectKey};
pub use object::{
    Bucket, CompletedPart, DeleteMarker, MultipartUpload, MultipartUploadState, ObjectMetadata,
    ObjectVersion, ObjectVersionRecord, PayloadFormat, UploadedPart,
};
pub use preview::{CONTENT_SIGNATURE_PROBE_BYTES, PreviewKind, content_signature_matches};
pub use quota::{BucketQuota, ByteQuota, ExpirationDays, ObjectCountQuota, VersioningState};
pub use range::{ByteRange, PartNumber, ResolvedByteRange};
pub use shard::{ShardIndex, ShardKind, ShardState};
