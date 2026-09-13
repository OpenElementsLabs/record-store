//! The `Range` header, resolved against an object size.
//!
//! A range is arithmetic on caller-supplied numbers against a size the caller
//! does not control, which is where off-by-one and overflow bugs turn into
//! reads past the end of an object. The invariant is absolute: whatever the
//! header says, an accepted range must lie inside the object.

#![no_main]

use libfuzzer_sys::fuzz_target;
use record_store_s3::fuzzing;

fuzz_target!(|input: (u64, &str)| {
    let (size, header) = input;

    let Some((offset, length)) = fuzzing::range_offset_and_length(header, size) else {
        return;
    };

    assert!(length > 0, "accepted an empty range from {header:?}");
    assert!(
        offset < size,
        "accepted a range starting at {offset} in an object of {size} bytes, from {header:?}"
    );
    let end = offset
        .checked_add(length)
        .unwrap_or_else(|| panic!("range {offset}+{length} from {header:?} overflows u64"));
    assert!(
        end <= size,
        "accepted a range covering bytes {offset}..{end} of an object of {size} bytes, \
         from {header:?}"
    );
});
