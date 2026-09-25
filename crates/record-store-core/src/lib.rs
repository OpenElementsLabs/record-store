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
mod mutation_event;
mod names;
mod object;
mod object_lock;
mod preview;
mod quota;
mod range;
mod redb_open;
mod shard;
mod storage_class;
mod trusted_proxy;

pub use checksum::{Checksum, ChecksumAlgorithm, ETag};
pub use durability::{DurabilityProfile, ErasureProfile, ReplicationProfile};
pub use error::{CoreError, ErrorCategory};
pub use ids::{
    AuditEventId, BucketId, ClusterId, ClusterOperationId, CredentialId, DeviceId, EmbedLinkId,
    EventId, JoinTokenId, LifecycleRuleId, NodeCredentialId, NodeId, ObjectId, OrganizationId,
    PolicyId, ReplicaTaskId, ServiceAccountId, ShardId, ShareLinkId, StripeId, UploadId, VersionId,
    WebhookId,
};
pub use lifecycle::{LifecycleRule, StorageUsage};
pub use mutation_event::{MutationEvent, StorageEventType, WriteOrigin};
pub use names::{BucketName, ObjectKey};
pub use object::{
    Bucket, CompletedPart, DeleteMarker, MultipartUpload, MultipartUploadState, ObjectMetadata,
    ObjectVersion, ObjectVersionRecord, PayloadFormat, UploadedPart,
};
pub use object_lock::{
    DefaultRetention, LockBlock, LockChangeRefused, ObjectLockConfiguration, ObjectLockState,
    Retention, RetentionMode, RetentionPeriod,
};
pub use preview::{CONTENT_SIGNATURE_PROBE_BYTES, PreviewKind, content_signature_matches};
pub use quota::{BucketQuota, ByteQuota, ExpirationDays, ObjectCountQuota, VersioningState};
pub use range::{ByteRange, PartNumber, ResolvedByteRange};
pub use redb_open::{DEFAULT_CACHE_BYTES, open_database, open_database_with_cache};
pub use shard::{ShardIndex, ShardKind, ShardState};
pub use storage_class::StorageClass;
pub use trusted_proxy::TrustedProxies;
