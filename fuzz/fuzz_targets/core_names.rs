//! Bucket-name and object-key validation.
//!
//! These two functions are the boundary between a caller's string and a name
//! the rest of the system trusts. An object key in particular is never
//! interpreted as a filesystem path, and the reason that holds is this
//! validator: a key containing `..`, an empty segment, or a backslash must
//! never be accepted, because every layer downstream is written on the
//! assumption that one cannot be.

#![no_main]

use libfuzzer_sys::fuzz_target;
use record_store_core::{BucketName, ObjectKey};

fuzz_target!(|data: &[u8]| {
    let Ok(value) = std::str::from_utf8(data) else {
        return;
    };

    if let Ok(key) = ObjectKey::new(value) {
        let key = key.as_str();
        assert_eq!(key, value, "object key was silently rewritten");
        assert!(
            key.len() <= ObjectKey::MAX_LENGTH,
            "accepted a {}-byte key",
            key.len()
        );
        assert!(!key.starts_with('/'), "accepted an absolute key: {key:?}");
        assert!(!key.contains('\\'), "accepted a backslash in {key:?}");
        assert!(
            !key.chars().any(char::is_control),
            "accepted a control character in {key:?}"
        );
        assert!(
            !key.split('/')
                .any(|segment| segment.is_empty() || segment == "." || segment == ".."),
            "accepted a traversable or ambiguous segment in {key:?}"
        );
    }

    if let Ok(bucket) = BucketName::new(value) {
        let bucket = bucket.as_str();
        assert_eq!(bucket, value, "bucket name was silently rewritten");
        assert!(
            (BucketName::MIN_LENGTH..=BucketName::MAX_LENGTH).contains(&bucket.len()),
            "accepted a {}-byte bucket name",
            bucket.len()
        );
        // A name that is also a valid host label is what keeps virtual-host
        // style addressing unambiguous, and a name that parses as an IPv4
        // address would make a bucket URL indistinguishable from an address.
        assert!(
            bucket.bytes().all(|byte| byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || byte == b'-'
                || byte == b'.'),
            "accepted a character outside the host-label set in {bucket:?}"
        );
        assert!(
            bucket.parse::<std::net::Ipv4Addr>().is_err(),
            "accepted IP address notation: {bucket:?}"
        );
        assert!(
            !bucket.contains(".."),
            "accepted adjacent periods in {bucket:?}"
        );
    }
});
