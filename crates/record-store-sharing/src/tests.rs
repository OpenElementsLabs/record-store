use std::str::FromStr;
use std::sync::Arc;

use chrono::{Duration, Utc};
use record_store_core::{BucketId, BucketName, ObjectKey, ShareLinkId, VersionId};

use tempfile::tempdir;

use super::*;

const KEY: &[u8] = b"capability-test-master-key-at-least-32-bytes";

async fn service(policy: SharingPolicy) -> SharingService {
    let directory = tempdir().expect("temporary directory");
    let store = CapabilityStore::open(directory.path().join("sharing.redb"), KEY)
        .await
        .expect("open store");
    // The directory is intentionally leaked for the lifetime of the test
    // process so the open database file outlives this helper.
    std::mem::forget(directory);
    SharingService::new(store, policy, TicketIssuer::derive(KEY).expect("tickets"))
}

fn share_request() -> CreateShareRequest {
    CreateShareRequest {
        label: "Board review".to_owned(),
        bucket_id: BucketId::new(),
        bucket: BucketName::from_str("reports").expect("bucket"),
        key: ObjectKey::new("q1/summary.pdf").expect("key"),
        version: VersionMode::FollowCurrent,
        permission: SharePermission::ViewAndDownload,
        expires_at: None,
        password: None,
        maximum_access_count: None,
        created_by: "management:system-administrator".to_owned(),
    }
}

fn embed_request(content_type: &str) -> CreateEmbedRequest {
    CreateEmbedRequest {
        label: "Company website".to_owned(),
        bucket_id: BucketId::new(),
        bucket: BucketName::from_str("assets").expect("bucket"),
        key: ObjectKey::new("brand/logo.png").expect("key"),
        version: VersionMode::FollowCurrent,
        expires_at: None,
        allowed_origins: vec!["https://example.com".to_owned()],
        disposition: EmbedDisposition::Inline,
        content_type: Some(content_type.to_owned()),
        created_by: "management:system-administrator".to_owned(),
    }
}

#[tokio::test]
async fn a_created_share_authorizes_access_and_a_revoked_one_never_does_again() {
    let service = service(SharingPolicy::default()).await;
    let now = Utc::now();
    let issued = service
        .create_share(share_request(), now)
        .await
        .expect("create share");

    let granted = service
        .authorize_share_access(&issued.token, None, false, "10.0.0.1", now)
        .await
        .expect("authorize");
    assert!(granted.is_ok());

    service
        .revoke_share(issued.link.id, now)
        .await
        .expect("revoke");

    let denied = service
        .authorize_share_access(&issued.token, None, false, "10.0.0.1", now)
        .await
        .expect("authorize");
    assert_eq!(
        denied.err(),
        Some(AccessDenial::Unavailable(AccessRefusal::NotUsable(
            CapabilityStatus::Revoked
        )))
    );
}

#[tokio::test]
async fn an_expired_share_stops_working_without_any_administrative_action() {
    let service = service(SharingPolicy::default()).await;
    let now = Utc::now();
    let mut request = share_request();
    request.expires_at = Some(now + Duration::hours(1));
    let issued = service.create_share(request, now).await.expect("create");

    assert!(
        service
            .authorize_share_access(&issued.token, None, false, "10.0.0.1", now)
            .await
            .expect("authorize")
            .is_ok()
    );
    let later = now + Duration::hours(2);
    assert_eq!(
        service
            .authorize_share_access(&issued.token, None, false, "10.0.0.1", later)
            .await
            .expect("authorize")
            .err(),
        Some(AccessDenial::Unavailable(AccessRefusal::NotUsable(
            CapabilityStatus::Expired
        )))
    );
}

#[tokio::test]
async fn an_access_budget_is_a_real_ceiling() {
    let service = service(SharingPolicy::default()).await;
    let now = Utc::now();
    let mut request = share_request();
    request.maximum_access_count = Some(2);
    let issued = service.create_share(request, now).await.expect("create");

    for attempt in 0..2 {
        assert!(
            service
                .authorize_share_access(&issued.token, None, true, "10.0.0.1", now)
                .await
                .expect("authorize")
                .is_ok(),
            "delivery {attempt} should be permitted"
        );
    }
    assert_eq!(
        service
            .authorize_share_access(&issued.token, None, true, "10.0.0.1", now)
            .await
            .expect("authorize")
            .err(),
        Some(AccessDenial::Unavailable(AccessRefusal::NotUsable(
            CapabilityStatus::Exhausted
        )))
    );
}

#[tokio::test]
async fn concurrent_deliveries_cannot_overspend_an_access_budget() {
    let service = Arc::new(service(SharingPolicy::default()).await);
    let now = Utc::now();
    let mut request = share_request();
    request.maximum_access_count = Some(5);
    let issued = Arc::new(service.create_share(request, now).await.expect("create"));

    let mut handles = Vec::new();
    for _ in 0..32 {
        let service = Arc::clone(&service);
        let issued = Arc::clone(&issued);
        handles.push(tokio::spawn(async move {
            service
                .authorize_share_access(&issued.token, None, true, "10.0.0.1", now)
                .await
                .expect("authorize")
                .is_ok()
        }));
    }
    let mut granted = 0;
    for handle in handles {
        if handle.await.expect("join") {
            granted += 1;
        }
    }
    assert_eq!(granted, 5, "the ceiling must hold under concurrency");
}

#[tokio::test]
async fn a_password_protected_share_discloses_nothing_before_it_is_unlocked() {
    let service = service(SharingPolicy::default()).await;
    let now = Utc::now();
    let mut request = share_request();
    request.password = Some("open sesame please".to_owned());
    let issued = service.create_share(request, now).await.expect("create");

    match service
        .look_up_share(&issued.token, None, "10.0.0.1", now)
        .await
        .expect("lookup")
    {
        ShareLookup::PasswordRequired(id) => assert_eq!(id, issued.link.id),
        other => panic!("expected a password challenge, got {other:?}"),
    }
    assert_eq!(
        service
            .authorize_share_access(&issued.token, None, false, "10.0.0.1", now)
            .await
            .expect("authorize")
            .err(),
        Some(AccessDenial::PasswordRequired)
    );

    let wrong = service
        .unlock_share(&issued.token, "not the password", "10.0.0.1", now)
        .await
        .expect("unlock");
    assert_eq!(wrong.err(), Some(UnlockFailure::IncorrectPassword));

    let ticket = service
        .unlock_share(&issued.token, "open sesame please", "10.0.0.1", now)
        .await
        .expect("unlock")
        .expect("correct password");
    assert!(
        service
            .authorize_share_access(&issued.token, Some(ticket.as_str()), false, "10.0.0.1", now)
            .await
            .expect("authorize")
            .is_ok()
    );

    // The same ticket also opens the descriptor, so a visitor who has just
    // entered the password is not challenged again by the request that is
    // meant to show them the file.
    match service
        .look_up_share(&issued.token, Some(ticket.as_str()), "10.0.0.1", now)
        .await
        .expect("lookup")
    {
        ShareLookup::Open(link) => assert_eq!(link.id, issued.link.id),
        other => panic!("expected an unlocked share, got {other:?}"),
    }
    // A ticket for a different share does not open this one.
    let stranger = service
        .tickets
        .issue(ShareLinkId::new(), now + Duration::hours(1));
    assert!(matches!(
        service
            .look_up_share(&issued.token, Some(stranger.as_str()), "10.0.0.1", now)
            .await
            .expect("lookup"),
        ShareLookup::PasswordRequired(_)
    ));
}

#[tokio::test]
async fn an_unlock_ticket_is_useless_against_a_different_share() {
    let service = service(SharingPolicy::default()).await;
    let now = Utc::now();
    let mut first = share_request();
    first.password = Some("open sesame please".to_owned());
    let first = service.create_share(first, now).await.expect("create");
    let mut second = share_request();
    second.password = Some("a different secret".to_owned());
    let second = service.create_share(second, now).await.expect("create");

    let ticket = service
        .unlock_share(&first.token, "open sesame please", "10.0.0.1", now)
        .await
        .expect("unlock")
        .expect("ticket");
    assert_eq!(
        service
            .authorize_share_access(&second.token, Some(ticket.as_str()), false, "10.0.0.1", now)
            .await
            .expect("authorize")
            .err(),
        Some(AccessDenial::PasswordRequired)
    );
}

#[tokio::test]
async fn repeated_password_guesses_are_throttled_per_share_and_client() {
    let policy = SharingPolicy {
        password_attempts_per_window: 3,
        ..SharingPolicy::default()
    };
    let service = service(policy).await;
    let now = Utc::now();
    let mut request = share_request();
    request.password = Some("open sesame please".to_owned());
    let issued = service.create_share(request, now).await.expect("create");

    for _ in 0..3 {
        assert_eq!(
            service
                .unlock_share(&issued.token, "wrong", "10.0.0.1", now)
                .await
                .expect("unlock")
                .err(),
            Some(UnlockFailure::IncorrectPassword)
        );
    }
    assert!(matches!(
        service
            .unlock_share(&issued.token, "wrong", "10.0.0.1", now)
            .await
            .expect("unlock")
            .err(),
        Some(UnlockFailure::Throttled { .. })
    ));
    // Another visitor is unaffected: a public link must not be lockable.
    assert_eq!(
        service
            .unlock_share(&issued.token, "wrong", "10.0.0.2", now)
            .await
            .expect("unlock")
            .err(),
        Some(UnlockFailure::IncorrectPassword)
    );
}

#[tokio::test]
async fn a_view_only_share_refuses_download_and_the_reverse() {
    let service = service(SharingPolicy::default()).await;
    let now = Utc::now();
    let mut request = share_request();
    request.permission = SharePermission::View;
    let view_only = service.create_share(request, now).await.expect("create");
    assert_eq!(
        service
            .authorize_share_access(&view_only.token, None, true, "10.0.0.1", now)
            .await
            .expect("authorize")
            .err(),
        Some(AccessDenial::NotPermitted)
    );

    let mut request = share_request();
    request.permission = SharePermission::Download;
    let download_only = service.create_share(request, now).await.expect("create");
    assert_eq!(
        service
            .authorize_share_access(&download_only.token, None, false, "10.0.0.1", now)
            .await
            .expect("authorize")
            .err(),
        Some(AccessDenial::NotPermitted)
    );
}

#[tokio::test]
async fn an_unknown_token_is_indistinguishable_from_a_revoked_one_and_is_rate_limited() {
    let policy = SharingPolicy {
        token_probes_per_window: 2,
        ..SharingPolicy::default()
    };
    let service = service(policy).await;
    let now = Utc::now();
    let stranger = CapabilityToken::generate().expect("token");

    for _ in 0..2 {
        assert_eq!(
            service
                .authorize_share_access(&stranger, None, false, "10.0.0.9", now)
                .await
                .expect("authorize")
                .err(),
            Some(AccessDenial::Unavailable(AccessRefusal::Unknown))
        );
    }
    assert!(matches!(
        service
            .authorize_share_access(&stranger, None, false, "10.0.0.9", now)
            .await
            .expect("authorize")
            .err(),
        Some(AccessDenial::Throttled { .. })
    ));
}

#[tokio::test]
async fn a_valid_share_is_never_charged_against_the_probe_limiter() {
    let policy = SharingPolicy {
        token_probes_per_window: 2,
        ..SharingPolicy::default()
    };
    let service = service(policy).await;
    let now = Utc::now();
    let issued = service
        .create_share(share_request(), now)
        .await
        .expect("create");
    // A media player seeking through a file issues far more requests than
    // the probe allowance; none of them may be treated as abuse.
    for _ in 0..50 {
        assert!(
            service
                .authorize_share_access(&issued.token, None, false, "10.0.0.5", now)
                .await
                .expect("authorize")
                .is_ok()
        );
    }
}

#[tokio::test]
async fn embeds_reject_active_content_at_creation() {
    let service = service(SharingPolicy::default()).await;
    let now = Utc::now();
    for content_type in ["text/html", "image/svg+xml", "application/xml"] {
        let error = service
            .create_embed(embed_request(content_type), now)
            .await
            .expect_err("active content must be refused");
        assert!(matches!(error, SharingError::PolicyRefused(_)));
    }
    assert!(
        service
            .create_embed(embed_request("image/png"), now)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn an_embed_serves_a_permitted_origin_and_refuses_an_unlisted_one() {
    let service = service(SharingPolicy::default()).await;
    let now = Utc::now();
    let issued = service
        .create_embed(embed_request("image/png"), now)
        .await
        .expect("create embed");

    let (_, decision) = service
        .authorize_embed_access(&issued.token, Some("https://example.com"), "10.0.0.1", now)
        .await
        .expect("authorize")
        .expect("granted");
    assert_eq!(decision, OriginDecision::Allowed);

    assert_eq!(
        service
            .authorize_embed_access(&issued.token, Some("https://evil.test"), "10.0.0.1", now)
            .await
            .expect("authorize")
            .err(),
        Some(AccessDenial::OriginDenied)
    );

    // A non-browser client presents no origin and is still served, but
    // without a CORS grant.
    let (_, decision) = service
        .authorize_embed_access(&issued.token, None, "10.0.0.1", now)
        .await
        .expect("authorize")
        .expect("granted");
    assert_eq!(decision, OriginDecision::NoOriginPresented);
}

#[tokio::test]
async fn an_embed_s_origins_can_be_narrowed_and_every_entry_is_validated() {
    let service = service(SharingPolicy::default()).await;
    let now = Utc::now();
    let issued = service
        .create_embed(embed_request("image/png"), now)
        .await
        .expect("create embed");

    assert!(
        service
            .set_embed_origins(issued.link.id, &["javascript:alert(1)".to_owned()], now)
            .await
            .is_err()
    );
    let updated = service
        .set_embed_origins(issued.link.id, &["https://app.example.com".to_owned()], now)
        .await
        .expect("update")
        .expect("embed");
    assert_eq!(updated.allowed_origins.len(), 1);
    assert_eq!(
        service
            .authorize_embed_access(&issued.token, Some("https://example.com"), "10.0.0.1", now)
            .await
            .expect("authorize")
            .err(),
        Some(AccessDenial::OriginDenied)
    );
}

#[tokio::test]
async fn a_revoked_embed_stops_serving_bytes_immediately() {
    let service = service(SharingPolicy::default()).await;
    let now = Utc::now();
    let issued = service
        .create_embed(embed_request("image/png"), now)
        .await
        .expect("create embed");
    service
        .revoke_embed(issued.link.id, now)
        .await
        .expect("revoke");
    assert_eq!(
        service
            .authorize_embed_access(&issued.token, Some("https://example.com"), "10.0.0.1", now)
            .await
            .expect("authorize")
            .err(),
        Some(AccessDenial::Unavailable(AccessRefusal::NotUsable(
            CapabilityStatus::Revoked
        )))
    );
}

#[tokio::test]
async fn deployment_policy_bounds_lifetime_and_can_require_expiry_and_passwords() {
    let policy = SharingPolicy {
        maximum_lifetime: Some(Duration::days(7)),
        require_expiration: true,
        require_share_password: true,
        ..SharingPolicy::default()
    };
    let service = service(policy).await;
    let now = Utc::now();

    assert!(matches!(
        service.create_share(share_request(), now).await,
        Err(SharingError::PolicyRefused(_))
    ));

    let mut request = share_request();
    request.expires_at = Some(now + Duration::days(30));
    request.password = Some("open sesame please".to_owned());
    assert!(matches!(
        service.create_share(request, now).await,
        Err(SharingError::PolicyRefused(_))
    ));

    let mut request = share_request();
    request.expires_at = Some(now + Duration::days(3));
    assert!(matches!(
        service.create_share(request, now).await,
        Err(SharingError::PolicyRefused(_))
    ));

    let mut request = share_request();
    request.expires_at = Some(now + Duration::days(3));
    request.password = Some("open sesame please".to_owned());
    assert!(service.create_share(request, now).await.is_ok());
}

#[tokio::test]
async fn disabling_sharing_refuses_creation_rather_than_hiding_the_button() {
    let policy = SharingPolicy {
        shares_enabled: false,
        embeds_enabled: false,
        ..SharingPolicy::default()
    };
    let service = service(policy).await;
    let now = Utc::now();
    assert!(matches!(
        service.create_share(share_request(), now).await,
        Err(SharingError::PolicyRefused(_))
    ));
    assert!(matches!(
        service.create_embed(embed_request("image/png"), now).await,
        Err(SharingError::PolicyRefused(_))
    ));
}

#[tokio::test]
async fn a_capability_url_can_be_copied_again_by_an_authorized_administrator() {
    let service = service(SharingPolicy::default()).await;
    let now = Utc::now();
    let issued = service
        .create_share(share_request(), now)
        .await
        .expect("create");
    let revealed = service
        .store()
        .reveal_share_token(issued.link.id)
        .await
        .expect("reveal")
        .expect("token");
    assert_eq!(revealed.expose(), issued.token.expose());
}

#[tokio::test]
async fn a_deleted_share_is_gone_from_every_index() {
    let service = service(SharingPolicy::default()).await;
    let now = Utc::now();
    let request = share_request();
    let bucket_id = request.bucket_id;
    let key = request.key.clone();
    let issued = service.create_share(request, now).await.expect("create");

    assert_eq!(
        service
            .store()
            .list_shares_for_object(bucket_id, &key)
            .await
            .expect("list")
            .len(),
        1
    );
    assert!(
        service
            .store()
            .delete_share(issued.link.id)
            .await
            .expect("delete")
    );
    assert!(
        service
            .store()
            .list_shares_for_object(bucket_id, &key)
            .await
            .expect("list")
            .is_empty()
    );
    assert!(
        service
            .store()
            .resolve_share(issued.token.digest())
            .await
            .expect("resolve")
            .is_none()
    );
}

#[tokio::test]
async fn capabilities_are_listed_only_against_their_own_object() {
    let service = service(SharingPolicy::default()).await;
    let now = Utc::now();
    let bucket_id = BucketId::new();
    for key in ["reports/a.pdf", "reports/a.pdf.backup", "reports/b.pdf"] {
        let mut request = share_request();
        request.bucket_id = bucket_id;
        request.key = ObjectKey::new(key).expect("key");
        service.create_share(request, now).await.expect("create");
    }
    let listed = service
        .store()
        .list_shares_for_object(bucket_id, &ObjectKey::new("reports/a.pdf").expect("key"))
        .await
        .expect("list");
    assert_eq!(listed.len(), 1, "a key prefix must not match a longer key");
    assert_eq!(listed[0].target.key.as_str(), "reports/a.pdf");
}

#[tokio::test]
async fn a_pinned_share_records_the_exact_version_it_was_created_for() {
    let service = service(SharingPolicy::default()).await;
    let now = Utc::now();
    let version = VersionId::new();
    let mut request = share_request();
    request.version = VersionMode::Pinned {
        version_id: version,
    };
    let issued = service.create_share(request, now).await.expect("create");
    assert_eq!(pinned_version(&issued.link.target), Some(version));

    let resolved = service
        .store()
        .resolve_share(issued.token.digest())
        .await
        .expect("resolve")
        .expect("share");
    assert_eq!(resolved.target.version.pinned(), Some(version));
}

#[tokio::test]
async fn labels_are_validated_rather_than_stored_as_typed() {
    let service = service(SharingPolicy::default()).await;
    let now = Utc::now();
    for label in ["", "   ", "with\u{0}control"] {
        let mut request = share_request();
        request.label = label.to_owned();
        assert!(
            service.create_share(request, now).await.is_err(),
            "accepted label {label:?}"
        );
    }
    let mut request = share_request();
    request.label = "  Board review  ".to_owned();
    let issued = service.create_share(request, now).await.expect("create");
    assert_eq!(issued.link.label, "Board review");
}
