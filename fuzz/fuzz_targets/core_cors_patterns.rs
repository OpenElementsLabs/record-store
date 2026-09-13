//! CORS origin and header pattern matching.
//!
//! A `CorsPattern` decides whether a browser's `Origin` is allowed to read a
//! response, so a matching bug here is a cross-origin disclosure rather than a
//! crash. The pattern language has exactly one wildcard, and the property that
//! makes it safe to reason about is that a pattern without a `*` matches only
//! the string it spells -- a matcher that treated some other character as
//! special would grant an origin nobody configured.

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use record_store_core::{CorsMethod, CorsPattern};

#[derive(Arbitrary, Debug)]
struct Input<'a> {
    pattern: &'a str,
    candidate: &'a str,
}

fuzz_target!(|input: Input<'_>| {
    let Input { pattern, candidate } = input;

    if let Ok(origin) = CorsPattern::origin(pattern) {
        let text = origin.as_str();
        assert!(
            text == "*" || text.starts_with("http://") || text.starts_with("https://"),
            "accepted an origin pattern that is neither * nor an origin: {text:?}"
        );
        if !text.contains('*') {
            assert_eq!(
                origin.matches(candidate),
                text == candidate,
                "literal origin {text:?} matched {candidate:?} inexactly"
            );
        }
        // The bare wildcard is the one pattern that is meant to match anything,
        // including the empty origin. If it ever failed to, a configuration
        // that reads as "allow all" would quietly allow less than it says.
        if origin.is_wildcard() {
            assert!(
                origin.matches(candidate),
                "the bare wildcard did not match {candidate:?}"
            );
        }
    }

    if let Ok(header) = CorsPattern::header(pattern) {
        let text = header.as_str();
        if !text.contains('*') {
            assert_eq!(
                header.matches(candidate),
                text == candidate,
                "literal header {text:?} matched {candidate:?} inexactly"
            );
        }
    }

    let _ = CorsMethod::parse(pattern);
});
