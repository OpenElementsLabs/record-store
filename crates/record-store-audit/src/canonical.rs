//! Deterministic, versioned encoding of an audit event.
//!
//! A hash chain is only worth anything if the bytes being hashed are
//! reproducible, by this release and by an independent implementation years
//! from now. That rules out `serde_json`, which leaves string escaping, number
//! formatting, and the treatment of fields added later underdetermined — a
//! record could re-encode to different bytes after an innocuous dependency
//! bump, and every hash after it would stop verifying.
//!
//! So the encoding here is explicit and fixed. Every field appears in a
//! declared order, every variable-length value carries its length, and the
//! whole thing is prefixed with a version. Length prefixes matter more than
//! they look: without them, two different records could concatenate to the
//! same bytes, and a hash that cannot distinguish them proves nothing.
//!
//! Changing this encoding means adding a version, never editing version 1.
//! Records already on disk were hashed under the version they were written
//! with, and verification re-encodes them the same way.

use crate::{AuditEvent, AuditResult};

/// Encoding this module currently produces.
pub const CANONICAL_VERSION: u16 = 1;

/// Domain separator for a record hash.
///
/// Distinct from every other hash in the system so that a digest computed for
/// one purpose can never be replayed as a digest for another.
pub const RECORD_DOMAIN: &[u8] = b"record-store/audit-record/v1";

impl AuditResult {
    /// Returns the stable wire discriminant used by the canonical encoding.
    ///
    /// Written out rather than derived from declaration order, because
    /// reordering the enum would otherwise silently change every hash.
    const fn canonical_tag(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::Denied => 1,
            Self::Failure => 2,
            // Appended, never inserted. Renumbering an existing variant would
            // silently change the hash of every record already written with it.
            Self::Attempted => 3,
        }
    }
}

/// Appends a length-prefixed byte string.
fn put_bytes(out: &mut Vec<u8>, value: &[u8]) {
    // A record whose field exceeds 4 GiB cannot be produced by any call site,
    // and a saturating cast here would silently encode the wrong length. The
    // cast is checked so that such a record fails to encode rather than
    // encoding to something that verifies against different content.
    let length = u32::try_from(value.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(value);
}

/// Appends a length-prefixed string.
fn put_str(out: &mut Vec<u8>, value: &str) {
    put_bytes(out, value.as_bytes());
}

/// Appends an optional string as a presence tag followed by the value.
fn put_optional_str(out: &mut Vec<u8>, value: Option<&str>) {
    match value {
        Some(value) => {
            out.push(1);
            put_str(out, value);
        }
        // The tag distinguishes an absent field from a present empty one. They
        // are different facts, and a record must not be able to impersonate the
        // other by dropping a field.
        None => out.push(0),
    }
}

/// Returns the canonical bytes of an audit event.
#[must_use]
pub fn canonical_bytes(event: &AuditEvent) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    out.extend_from_slice(&CANONICAL_VERSION.to_be_bytes());
    out.extend_from_slice(event.event_id.as_uuid().as_bytes());
    out.extend_from_slice(&event.timestamp.timestamp_micros().to_be_bytes());
    put_optional_str(&mut out, event.request_id.as_deref());
    put_str(&mut out, &event.principal);
    match event.credential_id {
        Some(id) => {
            out.push(1);
            out.extend_from_slice(id.as_bytes());
        }
        None => out.push(0),
    }
    put_optional_str(&mut out, event.source_ip.as_deref());
    put_str(&mut out, &event.operation);
    put_str(&mut out, &event.resource);
    out.push(event.result.canonical_tag());
    // A BTreeMap iterates in key order, so the entries are already sorted and
    // two events with the same metadata encode identically regardless of the
    // order the keys were inserted in.
    let entries = u32::try_from(event.metadata.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&entries.to_be_bytes());
    for (key, value) in &event.metadata {
        put_str(&mut out, key);
        put_str(&mut out, value);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::{DateTime, Utc};
    use record_store_core::AuditEventId;
    use uuid::Uuid;

    use super::*;

    /// A fully populated event with every value fixed, so its encoding is a
    /// constant this test can pin.
    fn golden_event() -> AuditEvent {
        AuditEvent {
            event_id: AuditEventId::from_uuid(Uuid::from_u128(
                0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10,
            )),
            timestamp: DateTime::<Utc>::from_timestamp_micros(1_772_000_000_000_000)
                .expect("fixture timestamp"),
            request_id: Some("req-01".to_owned()),
            principal: "service_account:0f9c".to_owned(),
            credential_id: Some(Uuid::from_u128(0x1111_2222_3333_4444_5555_6666_7777_8888)),
            source_ip: Some("203.0.113.7".to_owned()),
            operation: "s3:DELETE".to_owned(),
            resource: "bucket:records/statement.pdf".to_owned(),
            result: AuditResult::Denied,
            metadata: BTreeMap::from([
                ("reason".to_owned(), "compliance_retention".to_owned()),
                ("version_id".to_owned(), "abc".to_owned()),
            ]),
        }
    }

    /// The whole chain rests on these bytes. If this constant ever has to
    /// change, every record already written stops verifying, so a change here
    /// means a new canonical version rather than an edited expectation.
    #[test]
    fn version_one_encodes_to_pinned_bytes() {
        let encoded = canonical_bytes(&golden_event());
        assert_eq!(
            hex::encode(&encoded),
            concat!(
                "0001",
                "0102030405060708090a0b0c0d0e0f10",
                "00064b9fe68ac000",
                "01",
                "00000006",
                "7265712d3031",
                "00000014",
                "736572766963655f6163636f756e743a30663963",
                "01",
                "11112222333344445555666677778888",
                "01",
                "0000000b",
                "3230332e302e3131332e37",
                "00000009",
                "73333a44454c455445",
                "0000001c",
                "6275636b65743a7265636f7264732f73746174656d656e742e706466",
                "01",
                "00000002",
                "00000006",
                "726561736f6e",
                "00000014",
                "636f6d706c69616e63655f726574656e74696f6e",
                "0000000a",
                "76657273696f6e5f6964",
                "00000003",
                "616263",
            )
        );
    }

    /// Encoding is a pure function of the value: same event, same bytes, every
    /// time and in every process.
    #[test]
    fn encoding_is_stable_across_repeated_calls() {
        let event = golden_event();
        let first = canonical_bytes(&event);
        for _ in 0..64 {
            assert_eq!(canonical_bytes(&event), first);
        }
    }

    /// Metadata insertion order must not reach the bytes. Two events an
    /// operator would call identical have to hash identically.
    #[test]
    fn metadata_insertion_order_does_not_change_the_encoding() {
        let mut ascending = golden_event();
        ascending.metadata = BTreeMap::new();
        ascending.metadata.insert("a".into(), "1".into());
        ascending.metadata.insert("b".into(), "2".into());

        let mut descending = golden_event();
        descending.metadata = BTreeMap::new();
        descending.metadata.insert("b".into(), "2".into());
        descending.metadata.insert("a".into(), "1".into());

        assert_eq!(canonical_bytes(&ascending), canonical_bytes(&descending));
    }

    /// An absent field and a present empty one are different facts. Without the
    /// presence tag a record could shed a field and still hash the same.
    #[test]
    fn an_absent_field_encodes_differently_from_an_empty_one() {
        let mut absent = golden_event();
        absent.request_id = None;
        let mut empty = golden_event();
        empty.request_id = Some(String::new());
        assert_ne!(canonical_bytes(&absent), canonical_bytes(&empty));

        let mut no_ip = golden_event();
        no_ip.source_ip = None;
        let mut empty_ip = golden_event();
        empty_ip.source_ip = Some(String::new());
        assert_ne!(canonical_bytes(&no_ip), canonical_bytes(&empty_ip));
    }

    /// Length prefixes exist so that field boundaries cannot be moved. Without
    /// them these two events would encode to the same concatenation.
    #[test]
    fn field_boundaries_cannot_be_shifted_between_adjacent_fields() {
        let mut first = golden_event();
        first.operation = "ab".to_owned();
        first.resource = "c".to_owned();
        let mut second = golden_event();
        second.operation = "a".to_owned();
        second.resource = "bc".to_owned();
        assert_ne!(canonical_bytes(&first), canonical_bytes(&second));
    }

    /// Every field has to reach the bytes. A field the encoding forgets is a
    /// field an editor can change without breaking any hash.
    #[test]
    fn changing_any_single_field_changes_the_encoding() {
        let base = canonical_bytes(&golden_event());

        let mut event = golden_event();
        event.event_id = AuditEventId::from_uuid(Uuid::from_u128(9));
        assert_ne!(canonical_bytes(&event), base, "event_id");

        let mut event = golden_event();
        event.timestamp += chrono::Duration::microseconds(1);
        assert_ne!(canonical_bytes(&event), base, "timestamp");

        let mut event = golden_event();
        event.request_id = Some("req-02".into());
        assert_ne!(canonical_bytes(&event), base, "request_id");

        let mut event = golden_event();
        event.principal = "root".into();
        assert_ne!(canonical_bytes(&event), base, "principal");

        let mut event = golden_event();
        event.credential_id = Some(Uuid::from_u128(1));
        assert_ne!(canonical_bytes(&event), base, "credential_id");

        let mut event = golden_event();
        event.source_ip = Some("198.51.100.1".into());
        assert_ne!(canonical_bytes(&event), base, "source_ip");

        let mut event = golden_event();
        event.operation = "s3:GET".into();
        assert_ne!(canonical_bytes(&event), base, "operation");

        let mut event = golden_event();
        event.resource = "bucket:other/key".into();
        assert_ne!(canonical_bytes(&event), base, "resource");

        let mut event = golden_event();
        event.result = AuditResult::Success;
        assert_ne!(canonical_bytes(&event), base, "result");

        let mut event = golden_event();
        event.metadata.insert("extra".into(), "1".into());
        assert_ne!(canonical_bytes(&event), base, "metadata");
    }

    /// The result discriminants are a wire format, not an implementation
    /// detail. Reordering the enum must not renumber them.
    #[test]
    fn result_discriminants_are_pinned() {
        assert_eq!(AuditResult::Success.canonical_tag(), 0);
        assert_eq!(AuditResult::Denied.canonical_tag(), 1);
        assert_eq!(AuditResult::Failure.canonical_tag(), 2);
    }
}
