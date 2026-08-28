use axum::http::{HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use record_store_core::PreviewKind;

use crate::dto::RequestId;
use crate::sharing::dto::EMBEDDABLE_CONTENT_TYPES;
use crate::sharing::management::client_identity;
use crate::sharing::public::{EMBED_CONTENT_POLICY, SHARE_CONTENT_POLICY};
use crate::sharing::support::share_unavailable;

#[test]
fn the_advertised_embeddable_types_are_exactly_the_ones_accepted() {
    for content_type in EMBEDDABLE_CONTENT_TYPES {
        let kind = PreviewKind::classify(Some(content_type));
        assert!(
            kind.allows_element_embed(),
            "{content_type} is advertised but not element-embeddable"
        );
        assert!(
            PreviewKind::canonical_content_type(content_type).is_some(),
            "{content_type} has no canonical form"
        );
    }
}

#[test]
fn client_identity_prefers_a_forwarded_address_and_sanitises_it() {
    let mut headers = header::HeaderMap::new();
    headers.insert(
        "x-forwarded-for",
        HeaderValue::from_static("203.0.113.7, 10.0.0.1"),
    );
    assert_eq!(client_identity(&headers, None), "203.0.113.7");

    let mut hostile = header::HeaderMap::new();
    hostile.insert(
        "x-forwarded-for",
        HeaderValue::from_static("not an address at all"),
    );
    assert_eq!(client_identity(&hostile, None), "unknown");

    let mut oversized = header::HeaderMap::new();
    oversized.insert(
        "x-forwarded-for",
        HeaderValue::from_str(&"1".repeat(200)).expect("header"),
    );
    assert_eq!(client_identity(&oversized, None), "unknown");

    assert_eq!(client_identity(&header::HeaderMap::new(), None), "unknown");
}

#[test]
fn every_unusable_share_state_produces_one_indistinguishable_answer() {
    let request_id = RequestId::new();
    let response = share_unavailable(request_id).into_response();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[test]
fn content_policies_deny_script_and_keep_stored_bytes_in_an_opaque_origin() {
    for policy in [SHARE_CONTENT_POLICY, EMBED_CONTENT_POLICY] {
        assert!(policy.contains("sandbox"), "{policy}");
        assert!(policy.contains("default-src 'none'"), "{policy}");
        assert!(!policy.contains("allow-scripts"), "{policy}");
        assert!(!policy.contains("allow-same-origin"), "{policy}");
        assert!(!policy.contains("unsafe-inline"), "{policy}");
    }
    // Only the share and preview surface is framed by Record Store itself.
    assert!(SHARE_CONTENT_POLICY.contains("frame-ancestors 'self'"));
}
