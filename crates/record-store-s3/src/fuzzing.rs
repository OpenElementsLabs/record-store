//! Entry points for the fuzz targets in `fuzz/`, behind the `fuzzing` feature.
//!
//! Every parser reached from here runs before a request is authenticated, on
//! bytes an anonymous caller chose. The modules holding them are private, and
//! should stay that way: this is a deliberately narrow, feature-gated window
//! rather than a widening of the crate's API. Each wrapper returns only plain
//! data so that no internal type escapes with it.
//!
//! Where validation is split between the deserialiser and the handler, the
//! wrapper runs both, so a target constrains the same path a request takes
//! rather than the half of it that happens to be in one function.

use crate::handlers::listing::{decode_query_component, parse_list_query};
use crate::response::parse_range;
use crate::sigv4::{ParsedAuthorization, ParsedPresign, canonical_query, parse_amz_date};
use crate::xml::{
    CompleteMultipartUploadDocument, CorsConfigurationDocument, VersioningConfigurationDocument,
};
use record_store_core::{CorsConfiguration, ETag, PartNumber};

/// Parses an `Authorization` header. Returns the signed-header names, which the
/// canonical request is built from.
pub fn authorization_signed_headers(value: &str) -> Option<Vec<String>> {
    ParsedAuthorization::parse(value)
        .ok()
        .map(|parsed| parsed.signed_headers)
}

/// Parses the `X-Amz-*` query parameters of a presigned URL. Returns the
/// requested lifetime in seconds.
pub fn presign_expiry_seconds(query: &str) -> Option<i64> {
    ParsedPresign::parse(query)
        .ok()
        .map(|parsed| parsed.expires)
}

/// Parses an `X-Amz-Date` header value.
pub fn amz_date_is_valid(value: &str) -> bool {
    parse_amz_date(value).is_ok()
}

/// Canonicalises a query string the way signature calculation does.
pub fn canonicalise_query(query: &str) -> String {
    canonical_query(query)
}

/// Percent-decodes one query-string component.
pub fn decode_component(value: &str) -> Option<String> {
    decode_query_component(value).ok()
}

/// Parses a `Range` header against an object of `size` bytes. Returns the
/// accepted offset and length.
pub fn range_offset_and_length(value: &str, size: u64) -> Option<(u64, u64)> {
    parse_range(value, size)
        .ok()
        .map(|range| (range.offset(), range.length()))
}

/// Parses a ListObjectsV2 query string.
pub fn list_query_is_accepted(query: &str) -> bool {
    parse_list_query(query).is_ok()
}

/// Deserialises a `CORSConfiguration` request body and applies the validation
/// that turns it into a stored configuration.
pub fn cors_configuration_rule_count(body: &[u8]) -> Option<usize> {
    let document: CorsConfigurationDocument = quick_xml::de::from_reader(body).ok()?;
    let configuration: CorsConfiguration = document.try_into().ok()?;
    Some(configuration.rules.len())
}

/// Deserialises a `VersioningConfiguration` request body and resolves the
/// status the same way the handler does. Returns whether versioning would be
/// switched on.
pub fn versioning_is_enabled(body: &[u8]) -> Option<bool> {
    let document: VersioningConfigurationDocument = quick_xml::de::from_reader(body).ok()?;
    match document.status.as_deref() {
        Some("Enabled") => Some(true),
        Some("Suspended") => Some(false),
        _ => None,
    }
}

/// Deserialises a `CompleteMultipartUpload` request body and applies the
/// per-part validation the handler applies. Returns the accepted part numbers.
///
/// The range check lives in the handler rather than in the deserialiser, so
/// stopping at `from_reader` would fuzz half of what a request actually runs.
pub fn completed_part_numbers(body: &[u8]) -> Option<Vec<u16>> {
    let document: CompleteMultipartUploadDocument = quick_xml::de::from_reader(body).ok()?;
    document
        .parts
        .into_iter()
        .map(|part| {
            let number = PartNumber::new(part.part_number).ok()?;
            ETag::new(part.etag.trim_matches('"').to_owned()).ok()?;
            Some(number.get())
        })
        .collect()
}
