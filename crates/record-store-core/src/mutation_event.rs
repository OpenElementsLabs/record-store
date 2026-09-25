//! The storage event a committed mutation owes its subscribers.
//!
//! An event published after a mutation commits can be lost: the process can die
//! in the window between the two, and the publish itself can fail. Either way
//! the change happened and nobody was told, which for an integration driving
//! downstream work is indistinguishable from the change never happening.
//!
//! So the *intent* to publish is written inside the transaction that commits
//! the mutation, in the catalog that is authoritative for it. A crash anywhere
//! after that leaves a durable row describing an event that is owed, and
//! delivery resumes from it. The identifier is allocated there too, so a
//! republish after a crash is the same event rather than a second one.
//!
//! These types live here, below both the catalog that writes the row and the
//! outbox that drains it, so neither has to depend on the other.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{EventId, VersionId};

/// Stable storage-event names intended for integrations.
///
/// Part of the published webhook contract, so the serialized spelling of each
/// variant is fixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StorageEventType {
    #[serde(rename = "bucket.created")]
    BucketCreated,
    #[serde(rename = "bucket.deleted")]
    BucketDeleted,
    #[serde(rename = "object.created")]
    ObjectCreated,
    #[serde(rename = "object.updated")]
    ObjectUpdated,
    #[serde(rename = "object.deleted")]
    ObjectDeleted,
    #[serde(rename = "object.restored")]
    ObjectRestored,
    #[serde(rename = "multipart.completed")]
    MultipartCompleted,
    #[serde(rename = "multipart.aborted")]
    MultipartAborted,
}

impl StorageEventType {
    /// Returns the wire name carried in the delivery header.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BucketCreated => "bucket.created",
            Self::BucketDeleted => "bucket.deleted",
            Self::ObjectCreated => "object.created",
            Self::ObjectUpdated => "object.updated",
            Self::ObjectDeleted => "object.deleted",
            Self::ObjectRestored => "object.restored",
            Self::MultipartCompleted => "multipart.completed",
            Self::MultipartAborted => "multipart.aborted",
        }
    }
}

/// Why a version was written.
///
/// The catalog cannot tell a copy from a restore from the assembly of a
/// multipart upload: all three publish a version and look identical once they
/// arrive. The caller knows, so it says, and the event a subscriber receives
/// keeps describing what actually happened.
///
/// Carried on the command rather than inferred, for the same reason every other
/// non-deterministic input is: the command has to mean the same thing wherever
/// it is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteOrigin {
    /// An ordinary upload.
    #[default]
    Direct,
    /// A server-side copy.
    Copy,
    /// The object a multipart upload assembled.
    MultipartCompletion,
    /// A historical version promoted back to current.
    Restore,
}

/// Namespace for deriving storage-event identifiers.
///
/// A fixed, arbitrary UUID. Its only job is to keep derived event identifiers
/// out of any other UUIDv5 namespace; it is not a secret and never changes,
/// because changing it would renumber every event a subscriber has already seen.
const EVENT_NAMESPACE: Uuid = Uuid::from_bytes([
    0x1f, 0x4a, 0x9c, 0x2e, 0x7b, 0x63, 0x4d, 0x18, 0x9a, 0x05, 0xc7, 0x3e, 0x51, 0x88, 0x2d, 0x60,
]);

/// One storage event a committed mutation owes, as recorded in the catalog.
///
/// The `sequence` is allocated inside the committing transaction, so the order
/// of these rows is commit order — which is the order subscribers see, and the
/// order a resumed drain continues from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationEvent {
    /// Position in commit order.
    pub sequence: u64,
    /// The identifier the event will carry, derived at commit time.
    ///
    /// Derived rather than allocated at publish time so that a drain interrupted
    /// and resumed republishes the same event instead of inventing a second one
    /// with the same content — and *derived* rather than random so that every
    /// member of a cluster computes the same identifier for the same committed
    /// mutation. A random identifier here would make the journal differ between
    /// members applying one log entry, and a subscriber would see the same event
    /// twice under two identities after a failover or a snapshot install.
    ///
    /// See [`MutationEvent::derive_id`].
    pub event_id: EventId,
    /// What happened.
    pub event_type: StorageEventType,
    /// When the mutation committed.
    pub occurred_at: DateTime<Utc>,
    /// Bucket name, as a subscriber knows it.
    pub bucket: String,
    /// Object key, for events that name one.
    pub key: Option<String>,
    /// Version the event refers to, when it refers to one.
    pub version_id: Option<VersionId>,
    /// Object size, for events that publish bytes.
    pub size: Option<u64>,
}

impl MutationEvent {
    /// Derives the identifier for an event from the content that defines it.
    ///
    /// Every input is already deterministic at the point a command is applied:
    /// the sequence comes from a counter inside the same transaction, and the
    /// rest is the command's own data. Two members applying one log entry
    /// therefore produce byte-identical journal rows.
    #[must_use]
    pub fn derive_id(
        sequence: u64,
        event_type: StorageEventType,
        occurred_at: DateTime<Utc>,
        bucket: &str,
        key: Option<&str>,
        version_id: Option<VersionId>,
    ) -> EventId {
        let mut material = Vec::with_capacity(96 + bucket.len());
        material.extend_from_slice(&sequence.to_be_bytes());
        material.extend_from_slice(event_type.as_str().as_bytes());
        material.push(0);
        material.extend_from_slice(&occurred_at.timestamp_micros().to_be_bytes());
        material.extend_from_slice(bucket.as_bytes());
        material.push(0);
        if let Some(key) = key {
            material.extend_from_slice(key.as_bytes());
        }
        material.push(0);
        if let Some(version_id) = version_id {
            material.extend_from_slice(version_id.as_uuid().as_bytes());
        }
        EventId::from_uuid(Uuid::new_v5(&EVENT_NAMESPACE, &material))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two members applying the same log entry must journal the same event, or
    /// the replicated state machine is not deterministic and a snapshot taken on
    /// one member disagrees with another member's replayed state.
    #[test]
    fn a_derived_event_identifier_depends_only_on_the_event() {
        let at = DateTime::from_timestamp(1_700_000_000, 0).expect("timestamp");
        let version = VersionId::new();
        let first = MutationEvent::derive_id(
            7,
            StorageEventType::ObjectCreated,
            at,
            "bucket",
            Some("key"),
            Some(version),
        );
        let second = MutationEvent::derive_id(
            7,
            StorageEventType::ObjectCreated,
            at,
            "bucket",
            Some("key"),
            Some(version),
        );
        assert_eq!(
            first, second,
            "the same event must derive the same identifier"
        );
    }

    /// Distinct events must not collide, or a subscriber deduplicating on the
    /// identifier would drop one of them.
    #[test]
    fn different_events_derive_different_identifiers() {
        let at = DateTime::from_timestamp(1_700_000_000, 0).expect("timestamp");
        let base = MutationEvent::derive_id(
            7,
            StorageEventType::ObjectCreated,
            at,
            "bucket",
            Some("key"),
            None,
        );
        let cases = [
            MutationEvent::derive_id(
                8,
                StorageEventType::ObjectCreated,
                at,
                "bucket",
                Some("key"),
                None,
            ),
            MutationEvent::derive_id(
                7,
                StorageEventType::ObjectDeleted,
                at,
                "bucket",
                Some("key"),
                None,
            ),
            MutationEvent::derive_id(
                7,
                StorageEventType::ObjectCreated,
                at,
                "other",
                Some("key"),
                None,
            ),
            MutationEvent::derive_id(
                7,
                StorageEventType::ObjectCreated,
                at,
                "bucket",
                Some("other"),
                None,
            ),
            MutationEvent::derive_id(7, StorageEventType::ObjectCreated, at, "bucket", None, None),
        ];
        for case in cases {
            assert_ne!(
                base, case,
                "distinct events must derive distinct identifiers"
            );
        }
    }

    /// The wire names are the webhook contract; a rename would silently break
    /// every subscriber filtering on them.
    #[test]
    fn event_type_names_are_pinned() {
        for (kind, name) in [
            (StorageEventType::BucketCreated, "bucket.created"),
            (StorageEventType::BucketDeleted, "bucket.deleted"),
            (StorageEventType::ObjectCreated, "object.created"),
            (StorageEventType::ObjectUpdated, "object.updated"),
            (StorageEventType::ObjectDeleted, "object.deleted"),
            (StorageEventType::ObjectRestored, "object.restored"),
            (StorageEventType::MultipartCompleted, "multipart.completed"),
            (StorageEventType::MultipartAborted, "multipart.aborted"),
        ] {
            assert_eq!(kind.as_str(), name);
            assert_eq!(
                serde_json::to_string(&kind).expect("encode"),
                format!("\"{name}\"")
            );
        }
    }

    #[test]
    fn an_unstated_origin_is_an_ordinary_write() {
        assert_eq!(WriteOrigin::default(), WriteOrigin::Direct);
        // A command written before origins existed decodes as one.
        #[derive(serde::Deserialize)]
        struct Holder {
            #[serde(default)]
            origin: WriteOrigin,
        }
        let holder: Holder = serde_json::from_str("{}").expect("decode");
        assert_eq!(holder.origin, WriteOrigin::Direct);
    }
}
