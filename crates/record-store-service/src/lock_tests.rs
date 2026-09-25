//! Object Lock policy at the service layer: bypass auditing and defaults.

use chrono::{Duration, Utc};
use record_store_audit::{AuditQuery, AuditRepository, AuditResult};
use record_store_core::{
    BucketName, ObjectKey, ObjectLockConfiguration, ObjectLockState, Retention, RetentionMode,
};

use crate::test_support::{body, services_with_audit};
use crate::{LockContext, RetentionStatus, ServiceError, ServicePutRequest};

async fn locked_bucket(services: &crate::Services, name: &str) -> BucketName {
    let bucket = BucketName::new(name).expect("bucket name");
    services
        .buckets
        .create_locked(bucket.clone(), None, true)
        .await
        .expect("create locked bucket");
    bucket
}

/// Every bypass writes durable records, including one that did not work. An
/// attempted override of a retention is exactly as interesting to an auditor as
/// a successful one.
///
/// Two records per bypass, not one: the first is written before the version can
/// be gone, so a crash during the delete leaves evidence that somebody
/// authorized an override rather than leaving nothing at all.
#[tokio::test]
async fn every_governance_bypass_is_recorded_whether_or_not_it_succeeds() {
    let (_directory, services, audit) = services_with_audit().await;
    let bucket = locked_bucket(&services, "records").await;
    let key = ObjectKey::new("draft.txt").expect("key");

    let governance = services
        .objects
        .put(ServicePutRequest {
            bucket: bucket.clone(),
            key: key.clone(),
            content_type: None,
            custom_metadata: std::collections::BTreeMap::new(),
            expected_checksum: None,
            object_lock: Some(ObjectLockState {
                retention: Some(Retention {
                    mode: RetentionMode::Governance,
                    retain_until: Utc::now() + Duration::days(30),
                }),
                legal_hold: false,
            }),
            body: body(b"draft"),
        })
        .await
        .expect("put under governance retention");

    let compliance = services
        .objects
        .put(ServicePutRequest {
            bucket: bucket.clone(),
            key: ObjectKey::new("final.txt").expect("key"),
            content_type: None,
            custom_metadata: std::collections::BTreeMap::new(),
            expected_checksum: None,
            object_lock: Some(ObjectLockState {
                retention: Some(Retention {
                    mode: RetentionMode::Compliance,
                    retain_until: Utc::now() + Duration::days(30),
                }),
                legal_hold: false,
            }),
            body: body(b"final"),
        })
        .await
        .expect("put under compliance retention");

    let context =
        LockContext::principal("service_account:auditor-test").with_governance_bypass(true);

    // A bypass that works.
    services
        .objects
        .delete_version(
            &bucket,
            key.clone(),
            governance.metadata.version_id,
            &context,
        )
        .await
        .expect("an authorized bypass releases a governance retention");

    // A bypass that does not, because compliance mode has none.
    let refused = services
        .objects
        .delete_version(
            &bucket,
            compliance.metadata.key.clone(),
            compliance.metadata.version_id,
            &context,
        )
        .await
        .expect_err("compliance mode has no bypass");
    assert!(
        matches!(refused, ServiceError::ObjectLocked(_)),
        "{refused}"
    );

    let page = audit
        .query(AuditQuery {
            operation: Some("object-lock.bypass-delete-version".into()),
            limit: 50,
            ..AuditQuery::default()
        })
        .await
        .expect("audit query");
    assert_eq!(
        page.events.len(),
        4,
        "each bypass is announced and then resolved: {:?}",
        page.events
    );
    let announced = page
        .events
        .iter()
        .filter(|event| event.result == AuditResult::Attempted)
        .count();
    assert_eq!(announced, 2, "both bypasses were announced before they ran");
    assert!(
        page.events
            .iter()
            .any(|event| event.result == AuditResult::Success)
    );
    assert!(
        page.events
            .iter()
            .any(|event| event.result == AuditResult::Denied)
    );
    for event in &page.events {
        assert_eq!(event.principal, "service_account:auditor-test");
        assert!(
            event.metadata.contains_key("version_id"),
            "a record names the version it concerns"
        );
    }
    // Each outcome names the announcement it resolves, so a reader pairs them
    // without having to guess from timestamps.
    for outcome in page
        .events
        .iter()
        .filter(|event| event.result != AuditResult::Attempted)
    {
        let named = outcome
            .metadata
            .get(record_store_audit::intent::INTENT_EVENT_ID)
            .expect("an outcome names its announcement");
        assert!(
            page.events
                .iter()
                .any(|event| &event.event_id.to_string() == named),
            "the announcement it names must be in the trail"
        );
    }
}

/// A bypass is only permitted where the evidence can be written. A deployment
/// with no durable trail refuses it rather than releasing a retained version
/// with nothing to show for it.
#[tokio::test]
async fn a_bypass_is_refused_when_no_durable_trail_exists_to_record_it() {
    let (_directory, services) = crate::test_support::services().await;
    let bucket = BucketName::new("unrecorded").expect("bucket");
    services
        .buckets
        .create_locked(bucket.clone(), None, true)
        .await
        .expect("create locked bucket");
    let key = ObjectKey::new("draft.txt").expect("key");
    let stored = services
        .objects
        .put(ServicePutRequest {
            bucket: bucket.clone(),
            key: key.clone(),
            content_type: None,
            custom_metadata: std::collections::BTreeMap::new(),
            expected_checksum: None,
            object_lock: Some(ObjectLockState {
                retention: Some(Retention {
                    mode: RetentionMode::Governance,
                    retain_until: Utc::now() + Duration::days(30),
                }),
                legal_hold: false,
            }),
            body: body(b"draft"),
        })
        .await
        .expect("put under governance retention");

    let refused = services
        .objects
        .delete_version(
            &bucket,
            key.clone(),
            stored.metadata.version_id,
            &LockContext::principal("service_account:nobody").with_governance_bypass(true),
        )
        .await
        .expect_err("a bypass with nowhere to record it must be refused");
    assert!(
        matches!(refused, ServiceError::BypassNotRecordable),
        "{refused}"
    );

    // And the version is still there, which is the point.
    services
        .objects
        .head_version(&bucket, key, stored.metadata.version_id)
        .await
        .expect("the retained version survives a refused bypass");
}

/// An operation that presents no bypass must not produce a bypass record, or
/// the audit trail stops meaning anything.
#[tokio::test]
async fn an_ordinary_delete_writes_no_bypass_record() {
    let (_directory, services, audit) = services_with_audit().await;
    let bucket = locked_bucket(&services, "records").await;
    let key = ObjectKey::new("plain.txt").expect("key");
    let stored = services
        .objects
        .put(ServicePutRequest {
            bucket: bucket.clone(),
            key: key.clone(),
            content_type: None,
            custom_metadata: std::collections::BTreeMap::new(),
            expected_checksum: None,
            object_lock: None,
            body: body(b"plain"),
        })
        .await
        .expect("put");

    services
        .objects
        .delete_version(
            &bucket,
            key,
            stored.metadata.version_id,
            &LockContext::principal("service_account:ordinary"),
        )
        .await
        .expect("an unlocked version deletes normally");

    let page = audit
        .query(AuditQuery {
            operation: Some("object-lock.bypass-delete-version".into()),
            limit: 50,
            ..AuditQuery::default()
        })
        .await
        .expect("audit query");
    assert!(page.events.is_empty(), "no bypass was exercised");
}

#[tokio::test]
async fn a_bucket_default_is_applied_to_writes_that_name_no_lock() {
    let (_directory, services, _audit) = services_with_audit().await;
    let bucket = locked_bucket(&services, "records").await;
    services
        .locks
        .set_bucket_configuration(
            &bucket,
            ObjectLockConfiguration {
                default_retention: Some(record_store_core::DefaultRetention {
                    mode: RetentionMode::Compliance,
                    period: record_store_core::RetentionPeriod::Days(7),
                }),
            },
        )
        .await
        .expect("set default retention");

    let stored = services
        .objects
        .put(ServicePutRequest {
            bucket: bucket.clone(),
            key: ObjectKey::new("auto.txt").expect("key"),
            content_type: None,
            custom_metadata: std::collections::BTreeMap::new(),
            expected_checksum: None,
            object_lock: None,
            body: body(b"auto"),
        })
        .await
        .expect("put");

    let state = services
        .objects
        .version_lock(stored.metadata.version_id)
        .await
        .expect("read lock");
    let retention = state
        .retention
        .expect("the bucket default applies to a write that named no lock");
    assert_eq!(retention.mode, RetentionMode::Compliance);
    assert!(retention.retain_until > Utc::now() + Duration::days(6));
}

/// A background worker is not a person exercising a permission, so it carries
/// no bypass no matter what it is scanning.
#[tokio::test]
async fn a_system_context_never_carries_a_bypass() {
    assert!(!LockContext::system("lifecycle").bypass_governance);
    assert_eq!(
        LockContext::system("lifecycle").principal,
        "system:lifecycle"
    );
}

/// A lock record outlives the retention it describes, so a report that treated
/// the record's presence as "still held" would overstate what is protected —
/// which is the one direction that matters for an auditor.
#[tokio::test]
async fn the_retention_report_separates_held_versions_from_elapsed_ones() {
    let (_directory, services, _audit) = services_with_audit().await;
    let bucket = locked_bucket(&services, "records").await;

    let held = services
        .objects
        .put(ServicePutRequest {
            bucket: bucket.clone(),
            key: ObjectKey::new("held.txt").expect("key"),
            content_type: None,
            custom_metadata: std::collections::BTreeMap::new(),
            expected_checksum: None,
            object_lock: Some(ObjectLockState {
                retention: Some(Retention {
                    mode: RetentionMode::Compliance,
                    retain_until: Utc::now() + Duration::days(30),
                }),
                legal_hold: false,
            }),
            body: body(b"held"),
        })
        .await
        .expect("put held");

    let elapsed = services
        .objects
        .put(ServicePutRequest {
            bucket: bucket.clone(),
            key: ObjectKey::new("elapsed.txt").expect("key"),
            content_type: None,
            custom_metadata: std::collections::BTreeMap::new(),
            expected_checksum: None,
            object_lock: Some(ObjectLockState {
                retention: Some(Retention {
                    mode: RetentionMode::Governance,
                    retain_until: Utc::now() - Duration::days(1),
                }),
                legal_hold: false,
            }),
            body: body(b"elapsed"),
        })
        .await
        .expect("put elapsed");

    let on_hold = services
        .objects
        .put(ServicePutRequest {
            bucket: bucket.clone(),
            key: ObjectKey::new("legal.txt").expect("key"),
            content_type: None,
            custom_metadata: std::collections::BTreeMap::new(),
            expected_checksum: None,
            object_lock: Some(ObjectLockState {
                retention: None,
                legal_hold: true,
            }),
            body: body(b"legal"),
        })
        .await
        .expect("put legal hold");

    let report = services.locks.retention_report().await.expect("report");

    assert_eq!(report.buckets.len(), 1);
    assert_eq!(report.buckets[0].bucket, "records");
    assert_eq!(report.versions.len(), 3);
    assert_eq!(
        report.held_count, 2,
        "the compliance retention and the hold"
    );
    assert!(!report.truncated);

    let find = |version_id| {
        report
            .versions
            .iter()
            .find(|entry| entry.version_id == version_id)
            .unwrap_or_else(|| panic!("version {version_id} missing from the report"))
    };

    let held_entry = find(held.metadata.version_id);
    assert_eq!(held_entry.status, RetentionStatus::Held);
    assert_eq!(held_entry.retention_mode.as_deref(), Some("COMPLIANCE"));
    assert!(held_entry.retain_until.is_some());
    assert_eq!(held_entry.key, "held.txt");

    let elapsed_entry = find(elapsed.metadata.version_id);
    assert_eq!(
        elapsed_entry.status,
        RetentionStatus::Elapsed,
        "a retention past its date no longer holds the version"
    );
    // It is still reported, because the record is still there and an auditor
    // asking what is retained wants to see it rather than have it vanish.
    assert_eq!(elapsed_entry.retention_mode.as_deref(), Some("GOVERNANCE"));

    let hold_entry = find(on_hold.metadata.version_id);
    assert_eq!(hold_entry.status, RetentionStatus::Held);
    assert!(hold_entry.legal_hold);
    assert!(
        hold_entry.retention_mode.is_none(),
        "a legal hold is not a retention"
    );
}

/// A deployment with no Object Lock anywhere must report an empty report rather
/// than failing, so an operator can tell "nothing retained" from "broken".
#[tokio::test]
async fn a_deployment_without_object_lock_reports_an_empty_retention_report() {
    let (_directory, services, _audit) = services_with_audit().await;
    services
        .buckets
        .create(BucketName::new("plain").expect("bucket name"))
        .await
        .expect("create bucket");

    let report = services.locks.retention_report().await.expect("report");

    assert!(report.buckets.is_empty());
    assert!(report.versions.is_empty());
    assert_eq!(report.held_count, 0);
    assert!(!report.truncated);
}

/// A bucket with Object Lock enabled but nothing locked yet still appears, so
/// an auditor sees the policy exists before anything exercises it.
#[tokio::test]
async fn a_locked_bucket_with_no_locked_versions_still_appears() {
    let (_directory, services, _audit) = services_with_audit().await;
    locked_bucket(&services, "records").await;

    let report = services.locks.retention_report().await.expect("report");

    assert_eq!(report.buckets.len(), 1);
    assert!(report.versions.is_empty());
    assert_eq!(report.held_count, 0);
}
