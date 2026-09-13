//! Query-string canonicalisation, which decides what a signature covers.
//!
//! `canonical_query` reduces a raw query string to the form both the client and
//! the server sign. Two query strings that a signature should distinguish must
//! not canonicalise to the same text, and -- the property asserted here -- the
//! canonical form of an already-canonical string must be itself. If it were
//! not, a request could be signed in one form, re-canonicalised into another on
//! arrival, and either be rejected while valid or, worse, verify against a
//! different set of parameters than the caller signed.

#![no_main]

use libfuzzer_sys::fuzz_target;
use record_store_s3::fuzzing;

fuzz_target!(|data: &[u8]| {
    let Ok(query) = std::str::from_utf8(data) else {
        return;
    };

    let canonical = fuzzing::canonicalise_query(query);
    let twice = fuzzing::canonicalise_query(&canonical);
    assert_eq!(
        canonical, twice,
        "canonicalisation is not idempotent for {query:?}"
    );

    if let Some(expires) = fuzzing::presign_expiry_seconds(query) {
        // A presigned URL that never expires, or that expires in the past, is
        // not something the parser may hand on to signature verification.
        assert!(
            (1..=604_800).contains(&expires),
            "accepted a presigned lifetime of {expires}s, outside 1..=604800"
        );
    }

    let _ = fuzzing::decode_component(query);
});
