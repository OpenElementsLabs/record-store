use proptest::prelude::*;
use uuid::Uuid;

use super::*;

#[test]
fn preview_classification_ignores_parameters_and_case() {
    assert_eq!(PreviewKind::classify(Some("IMAGE/PNG")), PreviewKind::Image);
    assert_eq!(
        PreviewKind::classify(Some("text/plain; charset=UTF-8")),
        PreviewKind::Text
    );
    assert_eq!(PreviewKind::classify(None), PreviewKind::Unsupported);
}

#[test]
fn active_content_types_are_classified_as_unsafe_rather_than_unsupported() {
    for content_type in [
        "text/html",
        "image/svg+xml",
        "application/xml",
        "application/javascript",
        "application/xhtml+xml",
    ] {
        assert_eq!(
            PreviewKind::classify(Some(content_type)),
            PreviewKind::UnsafeInline,
            "{content_type} must be refused as active content"
        );
    }
    assert!(!PreviewKind::UnsafeInline.allows_inline());
    assert!(!PreviewKind::UnsafeInline.allows_element_embed());
}

#[test]
fn canonical_content_types_strip_caller_supplied_parameters() {
    assert_eq!(
        PreviewKind::canonical_content_type("image/png; hostile=\"\r\ninjected\""),
        Some("image/png")
    );
    assert_eq!(
        PreviewKind::canonical_content_type("text/markdown"),
        Some("text/plain; charset=utf-8")
    );
    assert_eq!(PreviewKind::canonical_content_type("text/html"), None);
}

#[test]
fn declared_media_types_are_corroborated_by_leading_bytes() {
    assert!(content_signature_matches(
        "image/png",
        b"\x89PNG\r\n\x1a\n rest"
    ));
    assert!(!content_signature_matches(
        "image/png",
        b"<html><script>alert(1)</script>"
    ));
    assert!(content_signature_matches(
        "image/jpeg",
        &[0xFF, 0xD8, 0xFF, 0xE0]
    ));
    assert!(content_signature_matches("application/pdf", b"%PDF-1.7\n"));
    assert!(!content_signature_matches("application/pdf", b"MZ\x90\x00"));
    assert!(content_signature_matches(
        "video/mp4",
        b"\x00\x00\x00\x18ftypmp42"
    ));
    assert!(content_signature_matches(
        "audio/wav",
        b"RIFF\x00\x00\x00\x00WAVEfmt "
    ));
    assert!(!content_signature_matches(
        "audio/wav",
        b"RIFF\x00\x00\x00\x00WEBPVP8 "
    ));
}

#[test]
fn text_that_looks_like_markup_is_refused_even_when_labelled_text() {
    assert!(!content_signature_matches(
        "text/plain",
        b"  <!DOCTYPE html>"
    ));
    assert!(!content_signature_matches(
        "application/json",
        b"\xEF\xBB\xBF<svg onload=alert(1)>"
    ));
    assert!(content_signature_matches(
        "application/json",
        b"{\"safe\":true}"
    ));
}

#[test]
fn typed_identifiers_are_not_interchangeable() {
    let raw = Uuid::new_v4();
    let bucket = BucketId::from_uuid(raw);
    let object = ObjectId::from_uuid(raw);
    assert_eq!(bucket.to_string(), object.to_string());
    assert_eq!(bucket.as_uuid(), raw);
}

#[test]
fn object_keys_reject_path_traversal_and_ambiguous_paths() {
    for key in ["", "/root", "../secret", "a/../secret", "a//b", "a\\b"] {
        assert!(ObjectKey::new(key).is_err(), "accepted invalid key: {key}");
    }
    assert_eq!(
        ObjectKey::new("images/2026/photo.jpg")
            .expect("valid object key")
            .as_str(),
        "images/2026/photo.jpg"
    );
}

#[test]
fn bucket_names_follow_safe_s3_constraints() {
    for name in [
        "",
        "ab",
        "UPPERCASE",
        "-leading",
        "trailing-",
        "192.168.1.1",
        "a..b",
        "record-store-system",
        "xn--reserved",
    ] {
        assert!(
            BucketName::new(name).is_err(),
            "accepted invalid name: {name}"
        );
    }
    assert_eq!(
        BucketName::new("photos-2026.example")
            .expect("valid bucket")
            .as_str(),
        "photos-2026.example"
    );
}

#[test]
fn checksum_round_trips_through_its_stable_text_form() {
    let checksum = Checksum::sha256([0xab; 32]);
    let encoded = checksum.to_string();
    assert_eq!(
        encoded.parse::<Checksum>().expect("valid checksum"),
        checksum
    );
    assert!("sha256:abcd".parse::<Checksum>().is_err());
    assert!(
        format!("md5:{}", "00".repeat(16))
            .parse::<Checksum>()
            .is_err()
    );
}

#[test]
fn byte_ranges_are_checked_and_clamped() {
    assert!(ByteRange::new(0, 0).is_err());
    assert!(ByteRange::new(u64::MAX, 2).is_err());
    assert!(ByteRange::new(10, 1).expect("range").resolve(10).is_err());
    assert_eq!(
        ByteRange::new(5, 20)
            .expect("range")
            .resolve(10)
            .expect("resolved"),
        ResolvedByteRange {
            offset: 5,
            length: 5
        }
    );
}

proptest! {
    #[test]
    fn accepted_bucket_names_always_satisfy_storage_safety_invariants(value in any::<String>()) {
        if let Ok(name) = BucketName::new(value) {
            let value = name.as_str();
            prop_assert!((BucketName::MIN_LENGTH..=BucketName::MAX_LENGTH).contains(&value.len()));
            prop_assert!(value.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'.')));
            prop_assert!(!value.contains(".."));
            prop_assert!(!value.starts_with('-') && !value.starts_with('.'));
            prop_assert!(!value.ends_with('-') && !value.ends_with('.'));
            prop_assert!(value.parse::<std::net::Ipv4Addr>().is_err());
        }
    }

    #[test]
    fn accepted_object_keys_never_contain_unsafe_path_segments(value in any::<String>()) {
        if let Ok(key) = ObjectKey::new(value) {
            let value = key.as_str();
            prop_assert!(!value.starts_with('/'));
            prop_assert!(!value.contains('\\'));
            prop_assert!(!value.chars().any(char::is_control));
            prop_assert!(value.split('/').all(|segment| !segment.is_empty() && segment != "." && segment != ".."));
            prop_assert!(value.len() <= ObjectKey::MAX_LENGTH);
        }
    }
}
