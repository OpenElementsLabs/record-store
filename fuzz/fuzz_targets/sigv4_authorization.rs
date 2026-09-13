//! The `Authorization` and `X-Amz-Date` headers of an unauthenticated request.
//!
//! Both are parsed before any signature is checked, which means every byte
//! reaching them was chosen by an anonymous caller. Rejecting a malformed
//! header is the expected outcome; panicking on one would let that caller stop
//! the process without holding a credential.

#![no_main]

use libfuzzer_sys::fuzz_target;
use record_store_s3::fuzzing;

fuzz_target!(|data: &[u8]| {
    let Ok(value) = std::str::from_utf8(data) else {
        return;
    };

    if let Some(signed_headers) = fuzzing::authorization_signed_headers(value) {
        // The signed-header list is folded into the string that the signature
        // is computed over. An empty name would make two different requests
        // canonicalise identically, so an accepted header must not carry one.
        assert!(
            signed_headers.iter().all(|name| !name.is_empty()),
            "accepted an Authorization header with an empty signed-header name: {value:?}"
        );
    }

    let _ = fuzzing::amz_date_is_valid(value);
});
