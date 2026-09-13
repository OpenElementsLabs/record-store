//! The three XML request bodies Record Store deserialises.
//!
//! Each is read from a caller's request body before anything in it has been
//! believed, so the XML reader and the serde types behind it are the widest
//! piece of attack surface in the S3 adapter. A malformed document has to come
//! back as an error, never as a panic or an unbounded allocation.

#![no_main]

use libfuzzer_sys::fuzz_target;
use record_store_s3::fuzzing;

fuzz_target!(|body: &[u8]| {
    // CORSConfiguration carries the validation that turns a document into an
    // authorization decision, so it is fuzzed through that conversion rather
    // than stopping at deserialisation.
    if let Some(count) = fuzzing::cors_configuration_rule_count(body) {
        assert!(
            count <= record_store_core::MAXIMUM_CORS_RULES,
            "accepted a CORS configuration of {count} rules, over the documented cap"
        );
    }

    let _ = fuzzing::versioning_is_enabled(body);

    // A completed multipart upload names the parts to assemble. Part numbers
    // are the index into stored state, so an accepted document must not contain
    // one outside the range S3 defines.
    if let Some(parts) = fuzzing::completed_part_numbers(body) {
        for part_number in parts {
            assert!(
                (1..=10_000).contains(&part_number),
                "accepted part number {part_number}, outside 1..=10000"
            );
        }
    }
});
