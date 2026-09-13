//! The ListObjectsV2 query string.
//!
//! Listing is reachable with a caller-chosen prefix, delimiter, continuation
//! token, and key count, all percent-encoded. The decoder and the parameter
//! parser run before the listing itself, on a string that has been checked for
//! nothing.

#![no_main]

use libfuzzer_sys::fuzz_target;
use record_store_s3::fuzzing;

fuzz_target!(|data: &[u8]| {
    let Ok(query) = std::str::from_utf8(data) else {
        return;
    };

    let _ = fuzzing::list_query_is_accepted(query);

    // Percent-decoding either yields text or reports a malformed escape. It can
    // only ever shrink its input -- three bytes of escape become one -- so a
    // decoded component that grew means the decoder invented bytes, and the
    // length a caller can reach past a size limit is no longer bounded by the
    // length they sent.
    if let Some(decoded) = fuzzing::decode_component(query) {
        assert!(
            decoded.len() <= query.len(),
            "decoding {query:?} produced {} bytes from {}",
            decoded.len(),
            query.len()
        );
    }
});
