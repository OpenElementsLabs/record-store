//! The hash chain linking audit records to one another.
//!
//! Each record commits to the one before it, so a record cannot be edited,
//! removed, or reordered without breaking every link after it. What that buys
//! is stated precisely in the security documentation and is worth repeating
//! here: the chain detects accidental corruption and after-the-fact edits by
//! anyone who cannot rewrite the whole store. It does not, on its own, detect
//! an operator who can rewrite every record and every hash. Only an external
//! anchor over a checkpoint makes a past state provable against that operator.

use sha2::{Digest, Sha256};

use crate::canonical::{RECORD_DOMAIN, canonical_bytes};
use crate::{AuditError, AuditEvent};

/// A SHA-256 digest used as a chain or tree link.
pub type Digest32 = [u8; 32];

/// Domain separator for the value a chain starts from.
///
/// A chain beginning at a constant of zeroes could be confused with one that
/// happens to contain a zero hash, and two deployments would start identically
/// even for different purposes. Deriving the genesis from its own separator
/// keeps the first link as distinguishable as every later one.
const GENESIS_DOMAIN: &[u8] = b"record-store/audit-genesis/v1";

/// Returns the value the first chained record links back to.
#[must_use]
pub fn genesis_hash() -> Digest32 {
    Sha256::digest(GENESIS_DOMAIN).into()
}

/// Returns the hash committing one record to its predecessor.
///
/// The sequence number is inside the digest, not merely adjacent to it, so a
/// record cannot be moved to another position and still verify.
#[must_use]
pub fn record_hash(sequence: u64, previous: &Digest32, canonical: &[u8]) -> Digest32 {
    let mut hasher = Sha256::new();
    hasher.update(RECORD_DOMAIN);
    hasher.update(sequence.to_be_bytes());
    hasher.update(previous);
    hasher.update(canonical);
    hasher.finalize().into()
}

/// Returns the hash of an event at a position, from the event itself.
#[must_use]
pub fn hash_event(sequence: u64, previous: &Digest32, event: &AuditEvent) -> Digest32 {
    record_hash(sequence, previous, &canonical_bytes(event))
}

/// One audit record with its position and its links.
///
/// Records written before this deployment had a hash chain carry no links at
/// all. They are kept and remain queryable, but they are not evidence of
/// anything and `chain` is `None` for them — see [`ChainLinks`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AuditRecord {
    /// Position in the log. Gapless and assigned on insert.
    pub sequence: u64,
    /// Chain links, absent for records that predate the chain.
    #[serde(default)]
    pub chain: Option<ChainLinks>,
    /// The event itself, exactly as the caller supplied it.
    pub event: AuditEvent,
}

/// The two digests that place a record in the chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChainLinks {
    /// Hash of the preceding record, or the genesis value for the first.
    #[serde(with = "digest_hex")]
    pub previous_hash: Digest32,
    /// Hash of this record.
    #[serde(with = "digest_hex")]
    pub record_hash: Digest32,
}

impl AuditRecord {
    /// Recomputes this record's hash and reports whether it still matches.
    ///
    /// An unchained record cannot disagree with a hash it never had, so it is
    /// reported separately rather than as a pass.
    #[must_use]
    pub fn verify_against(&self, previous: &Digest32) -> RecordVerdict {
        let Some(links) = self.chain else {
            return RecordVerdict::Unchained;
        };
        if links.previous_hash != *previous {
            return RecordVerdict::BrokenLink;
        }
        let expected = hash_event(self.sequence, previous, &self.event);
        if expected == links.record_hash {
            RecordVerdict::Intact
        } else {
            RecordVerdict::ContentChanged
        }
    }
}

/// What recomputing one record's hash established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordVerdict {
    /// The record hashes to what it claims, and links to its predecessor.
    Intact,
    /// The record's content no longer hashes to its stored digest.
    ContentChanged,
    /// The record does not link to the record that precedes it.
    BrokenLink,
    /// The record predates the chain and carries no links.
    Unchained,
    /// No record occupies this position, though the log runs past it.
    ///
    /// Never returned by [`AuditRecord::verify_against`], which is handed a
    /// record: it is a verdict about a *position*, reached by a walk that finds
    /// the sequence empty. Deletion is the one tampering the links alone cannot
    /// describe — the record that would have carried the broken link is the one
    /// that is gone — so the gapless sequence is what catches it.
    Missing,
}

impl RecordVerdict {
    /// Returns a stable reason string for reports and audit output.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Intact => "intact",
            Self::ContentChanged => "content changed",
            Self::BrokenLink => "broken link to the previous record",
            Self::Unchained => "predates the hash chain",
            Self::Missing => "record is missing from the log",
        }
    }
}

/// Serializes a digest as hex so stored records stay readable.
pub(crate) mod digest_hex {
    use serde::{Deserialize, Deserializer, Serializer};

    use super::Digest32;

    pub(crate) fn serialize<S: Serializer>(
        value: &Digest32,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&hex::encode(value))
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Digest32, D::Error> {
        let text = String::deserialize(deserializer)?;
        let bytes = hex::decode(&text).map_err(serde::de::Error::custom)?;
        Digest32::try_from(bytes.as_slice())
            .map_err(|_| serde::de::Error::custom("a digest must be 32 bytes"))
    }
}

/// Parses a digest from hex, for values arriving from a file or a CLI flag.
pub fn parse_digest(value: &str) -> Result<Digest32, AuditError> {
    let bytes = hex::decode(value.trim()).map_err(|_| AuditError::InvalidDigest)?;
    Digest32::try_from(bytes.as_slice()).map_err(|_| AuditError::InvalidDigest)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::{DateTime, Utc};
    use record_store_core::AuditEventId;
    use uuid::Uuid;

    use super::*;
    use crate::AuditResult;

    fn event(operation: &str) -> AuditEvent {
        AuditEvent {
            event_id: AuditEventId::from_uuid(Uuid::from_u128(1)),
            timestamp: DateTime::<Utc>::from_timestamp_micros(1_772_000_000_000_000)
                .expect("fixture timestamp"),
            request_id: None,
            principal: "system:test".into(),
            credential_id: None,
            source_ip: None,
            operation: operation.into(),
            resource: "bucket:records/a".into(),
            result: AuditResult::Success,
            metadata: BTreeMap::new(),
        }
    }

    fn chained(sequence: u64, previous: Digest32, operation: &str) -> AuditRecord {
        let event = event(operation);
        AuditRecord {
            sequence,
            chain: Some(ChainLinks {
                previous_hash: previous,
                record_hash: hash_event(sequence, &previous, &event),
            }),
            event,
        }
    }

    /// The genesis value is a constant of this format, so it is pinned like any
    /// other part of the wire format.
    #[test]
    fn the_genesis_hash_is_pinned_and_is_not_zero() {
        assert_eq!(
            hex::encode(genesis_hash()),
            hex::encode(Sha256::digest(b"record-store/audit-genesis/v1"))
        );
        assert_ne!(genesis_hash(), [0_u8; 32]);
    }

    #[test]
    fn an_untouched_chain_verifies_link_by_link() {
        let mut previous = genesis_hash();
        for sequence in 0..8_u64 {
            let record = chained(sequence, previous, "s3:PUT");
            assert_eq!(record.verify_against(&previous), RecordVerdict::Intact);
            previous = record.chain.expect("chained").record_hash;
        }
    }

    /// Editing a record after the fact is the thing this exists to catch.
    #[test]
    fn editing_a_record_breaks_its_own_hash() {
        let previous = genesis_hash();
        let mut record = chained(4, previous, "s3:PUT");
        record.event.operation = "s3:GET".into();
        assert_eq!(
            record.verify_against(&previous),
            RecordVerdict::ContentChanged
        );
    }

    /// Moving a record to another position must not verify, which is why the
    /// sequence number is inside the digest.
    #[test]
    fn a_record_cannot_be_moved_to_another_position() {
        let previous = genesis_hash();
        let mut record = chained(4, previous, "s3:PUT");
        record.sequence = 5;
        assert_eq!(
            record.verify_against(&previous),
            RecordVerdict::ContentChanged
        );
    }

    /// Splicing a record onto a different predecessor is reported as a broken
    /// link rather than changed content, because the two failures send an
    /// operator to different places.
    #[test]
    fn a_record_spliced_onto_a_different_predecessor_reports_a_broken_link() {
        let record = chained(1, genesis_hash(), "s3:PUT");
        let elsewhere = Sha256::digest(b"some other chain").into();
        assert_eq!(record.verify_against(&elsewhere), RecordVerdict::BrokenLink);
    }

    /// A record written before the chain existed carries no links, and must be
    /// reported as such rather than counted as verified.
    #[test]
    fn a_record_without_links_is_reported_as_unchained() {
        let record = AuditRecord {
            sequence: 0,
            chain: None,
            event: event("s3:PUT"),
        };
        assert_eq!(
            record.verify_against(&genesis_hash()),
            RecordVerdict::Unchained
        );
    }

    /// Two different events must not produce the same link, even at the same
    /// position after the same predecessor.
    #[test]
    fn different_events_at_the_same_position_hash_differently() {
        let previous = genesis_hash();
        assert_ne!(
            hash_event(1, &previous, &event("s3:PUT")),
            hash_event(1, &previous, &event("s3:GET"))
        );
    }

    #[test]
    fn digest_links_round_trip_through_serialization() {
        let record = chained(3, genesis_hash(), "s3:PUT");
        let encoded = serde_json::to_vec(&record).expect("encode");
        let decoded: AuditRecord = serde_json::from_slice(&encoded).expect("decode");
        assert_eq!(decoded, record);
    }

    #[test]
    fn a_digest_of_the_wrong_length_is_refused() {
        assert!(parse_digest(&hex::encode([1_u8; 32])).is_ok());
        assert!(parse_digest(&hex::encode([1_u8; 31])).is_err());
        assert!(parse_digest("not hex").is_err());
    }
}
