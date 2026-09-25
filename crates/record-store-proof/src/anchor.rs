//! Constants for the external anchor, defined ahead of the parser that needs
//! them.
//!
//! An RFC 3161 timestamp token names the digest algorithm it covers by object
//! identifier, so parsing a `TSTInfo` means comparing an OID read off the wire
//! with the one for SHA-256. That comparison is where the ASN.1 crates make
//! trouble: `cms 0.2` reaches `const-oid 0.9.6` through `der 0.7`, while `sha2
//! 0.11` reaches `const-oid 0.10.2` through `digest 0.11`, and the two
//! `ObjectIdentifier` types are unrelated to the compiler even though they
//! encode the same thing. A comparison between them does not fail at the
//! semicolon; it fails as a type error in the middle of parser work, or worse,
//! gets papered over by converting through a string.
//!
//! So SHA-256's identifier lives here, as bytes, belonging to neither crate.
//! The bytes are what any version of any ASN.1 library must produce, which is
//! the only definition that cannot go stale. See `docs/reference/audit-chain.md`
//! for the dependency decision this sits under.

/// SHA-256, in dotted form, as RFC 5754 assigns it.
pub const SHA256_OID_DOTTED: &str = "2.16.840.1.101.3.4.2.1";

/// The content octets of SHA-256's object identifier.
///
/// This is the value a DER decoder hands back from an `OBJECT IDENTIFIER`,
/// without the tag and length — the same bytes `ObjectIdentifier::as_bytes`
/// returns in either `const-oid` version.
pub const SHA256_OID_CONTENT: [u8; 9] = [0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01];

/// The complete DER encoding of SHA-256's object identifier.
///
/// Tag `0x06`, length `0x09`, then [`SHA256_OID_CONTENT`].
pub const SHA256_OID_DER: [u8; 11] = [
    0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodes dotted arcs per X.690 clause 8.19, so the constant above is
    /// checked against the rule rather than against another copy of itself.
    ///
    /// The first two arcs share one subidentifier as `40 * first + second`;
    /// every subidentifier is base-128, most significant group first, with the
    /// top bit set on all but the last octet.
    fn encode_dotted(dotted: &str) -> Vec<u8> {
        let arcs: Vec<u128> = dotted
            .split('.')
            .map(|arc| arc.parse().expect("an arc is a number"))
            .collect();
        assert!(
            arcs.len() >= 2,
            "an object identifier has at least two arcs"
        );

        let mut subidentifiers = vec![40 * arcs[0] + arcs[1]];
        subidentifiers.extend_from_slice(&arcs[2..]);

        let mut out = Vec::new();
        for value in subidentifiers {
            let mut groups = vec![u8::try_from(value & 0x7f).expect("seven bits fit in a byte")];
            let mut rest = value >> 7;
            while rest > 0 {
                groups.push(u8::try_from(rest & 0x7f).expect("seven bits fit in a byte") | 0x80);
                rest >>= 7;
            }
            groups.reverse();
            out.extend_from_slice(&groups);
        }
        out
    }

    /// The constant is what the encoding rules say, not what some library said
    /// on the day it was written down.
    #[test]
    fn the_sha256_identifier_matches_the_encoding_rules() {
        assert_eq!(encode_dotted(SHA256_OID_DOTTED), SHA256_OID_CONTENT);
    }

    /// The encoder above has to be right about something already known, or it
    /// proves nothing about the constant it checks.
    #[test]
    fn the_reference_encoder_agrees_with_published_identifiers() {
        // RFC 8017, id-sha512: 2.16.840.1.101.3.4.2.3
        assert_eq!(
            encode_dotted("2.16.840.1.101.3.4.2.3"),
            [0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03]
        );
        // RFC 3161, id-ct-TSTInfo: 1.2.840.113549.1.9.16.1.4, which exercises
        // a subidentifier needing three base-128 groups.
        assert_eq!(
            encode_dotted("1.2.840.113549.1.9.16.1.4"),
            [
                0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x09, 0x10, 0x01, 0x04
            ]
        );
    }

    /// The tag and length are part of what a `TSTInfo` carries, so they are
    /// pinned too: `OBJECT IDENTIFIER` is tag `0x06`, and DER lengths under
    /// 128 are a single octet.
    #[test]
    fn the_der_encoding_is_the_content_behind_its_tag_and_length() {
        assert_eq!(SHA256_OID_DER[0], 0x06);
        assert_eq!(usize::from(SHA256_OID_DER[1]), SHA256_OID_CONTENT.len());
        assert_eq!(SHA256_OID_DER[2..], SHA256_OID_CONTENT);
    }
}
