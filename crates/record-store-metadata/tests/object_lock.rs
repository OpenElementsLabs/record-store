//! Object Lock enforcement, the clock high-water mark, and the v4 migration.
//!
//! These exercise the catalog directly rather than through the S3 adapter,
//! because the guarantee being tested is that the *transaction* refuses — a
//! check that lived only in a protocol handler could be raced by a concurrent
//! retention change, or simply bypassed by another caller.

use std::collections::BTreeMap;

use chrono::{Duration, Utc};
use record_store_core::{
    Bucket, BucketId, BucketName, BucketQuota, Checksum, DefaultRetention, ETag, LockBlock,
    LockChangeRefused, ObjectId, ObjectKey, ObjectLockConfiguration, ObjectLockState,
    ObjectMetadata, OrganizationId, Retention, RetentionMode, RetentionPeriod, VersionId,
    VersioningState, WriteOrigin,
};
use record_store_metadata::{
    LockRelease, MetadataError, MetadataRepository, NewDeleteMarker, RedbMetadataRepository,
};

const TOLERANCE_SECONDS: u32 = 5;

fn release() -> LockRelease {
    LockRelease::new(Utc::now(), TOLERANCE_SECONDS)
}

fn locked_bucket(name: &str, default_retention: Option<DefaultRetention>) -> Bucket {
    Bucket {
        id: BucketId::new(),
        organization_id: OrganizationId::new(),
        name: BucketName::new(name).expect("bucket name"),
        created_at: Utc::now(),
        versioning: VersioningState::Enabled,
        quota: BucketQuota::default(),
        storage_class: None,
        durability_policy: None,
        object_lock: Some(ObjectLockConfiguration { default_retention }),
        cors: None,
    }
}

fn object(bucket: BucketId, key: &str) -> ObjectMetadata {
    let now = Utc::now();
    ObjectMetadata {
        id: ObjectId::new(),
        bucket_id: bucket,
        key: ObjectKey::new(key).expect("object key"),
        version_id: VersionId::new(),
        size: 11,
        checksum: Checksum::sha256([7; 32]),
        payload_format: record_store_core::PayloadFormat::Plaintext,
        durability: record_store_core::DurabilityProfile::Single,
        etag: ETag::from_md5([9; 16]),
        content_type: None,
        custom_metadata: BTreeMap::new(),
        created_at: now,
        modified_at: now,
    }
}

async fn catalog() -> (tempfile::TempDir, RedbMetadataRepository) {
    let directory = tempfile::tempdir().expect("temporary directory");
    let repository = RedbMetadataRepository::open(directory.path().join("catalog.redb"))
        .await
        .expect("catalog");
    (directory, repository)
}

/// Sets up a locked bucket holding one version under the given lock state.
async fn locked_version(
    state: ObjectLockState,
) -> (
    tempfile::TempDir,
    RedbMetadataRepository,
    Bucket,
    ObjectMetadata,
) {
    let (directory, repository) = catalog().await;
    let bucket = locked_bucket("records", None);
    repository.create_bucket(&bucket).await.expect("bucket");
    let metadata = object(bucket.id, "statement.pdf");
    repository
        .put_object(&metadata, Some(state), WriteOrigin::Direct)
        .await
        .expect("put object");
    (directory, repository, bucket, metadata)
}

#[tokio::test]
async fn a_version_under_compliance_retention_cannot_be_deleted_by_anyone() {
    let retain_until = Utc::now() + Duration::days(30);
    let (_directory, repository, bucket, metadata) = locked_version(ObjectLockState {
        retention: Some(Retention {
            mode: RetentionMode::Compliance,
            retain_until,
        }),
        legal_hold: false,
    })
    .await;

    // Both with and without a bypass: compliance mode has no escape hatch, and
    // the caller presenting one is not a caller who may ignore it.
    for bypass in [false, true] {
        let error = repository
            .delete_object_version(
                bucket.id,
                &metadata.key,
                metadata.version_id,
                release().with_governance_bypass(bypass),
            )
            .await
            .expect_err("a compliance retention must refuse the delete");
        assert!(
            matches!(
                error,
                MetadataError::VersionLocked(LockBlock::Retention {
                    mode: RetentionMode::Compliance,
                    ..
                })
            ),
            "bypass {bypass}: {error}"
        );
    }

    // And the version is genuinely still there, not merely reported as kept.
    assert!(
        repository
            .get_object_version(bucket.id, &metadata.key, metadata.version_id)
            .await
            .expect("read version")
            .is_some()
    );
}

#[tokio::test]
async fn a_governance_retention_yields_only_to_an_authorized_bypass() {
    let state = ObjectLockState {
        retention: Some(Retention {
            mode: RetentionMode::Governance,
            retain_until: Utc::now() + Duration::days(30),
        }),
        legal_hold: false,
    };
    let (_directory, repository, bucket, metadata) = locked_version(state).await;

    let refused = repository
        .delete_object_version(bucket.id, &metadata.key, metadata.version_id, release())
        .await
        .expect_err("no bypass presented");
    assert!(matches!(
        refused,
        MetadataError::VersionLocked(LockBlock::Retention {
            mode: RetentionMode::Governance,
            ..
        })
    ));

    let removed = repository
        .delete_object_version(
            bucket.id,
            &metadata.key,
            metadata.version_id,
            release().with_governance_bypass(true),
        )
        .await
        .expect("an authorized bypass releases a governance retention")
        .expect("the version existed");
    assert_eq!(removed.removed.version_id(), metadata.version_id);
}

#[tokio::test]
async fn a_legal_hold_blocks_deletion_in_either_mode_and_ignores_a_bypass() {
    for mode in [RetentionMode::Governance, RetentionMode::Compliance] {
        let state = ObjectLockState {
            // The retention is already elapsed, so the hold is doing all the work.
            retention: Some(Retention {
                mode,
                retain_until: Utc::now() - Duration::days(1),
            }),
            legal_hold: true,
        };
        let (_directory, repository, bucket, metadata) = locked_version(state).await;
        let error = repository
            .delete_object_version(
                bucket.id,
                &metadata.key,
                metadata.version_id,
                release().with_governance_bypass(true),
            )
            .await
            .expect_err("a legal hold has no bypass");
        assert!(
            matches!(error, MetadataError::VersionLocked(LockBlock::LegalHold)),
            "{mode:?}: {error}"
        );
    }
}

#[tokio::test]
async fn an_elapsed_retention_stops_blocking_the_delete() {
    let state = ObjectLockState {
        retention: Some(Retention {
            mode: RetentionMode::Compliance,
            retain_until: Utc::now() - Duration::seconds(1),
        }),
        legal_hold: false,
    };
    let (_directory, repository, bucket, metadata) = locked_version(state).await;
    repository
        .delete_object_version(bucket.id, &metadata.key, metadata.version_id, release())
        .await
        .expect("an elapsed retention holds nothing")
        .expect("the version existed");
}

/// The distinction the documentation makes: a delete marker hides the object
/// without touching the retained version underneath it, so it stays allowed.
#[tokio::test]
async fn a_delete_marker_is_allowed_over_a_retained_version() {
    let state = ObjectLockState {
        retention: Some(Retention {
            mode: RetentionMode::Compliance,
            retain_until: Utc::now() + Duration::days(30),
        }),
        legal_hold: true,
    };
    let (_directory, repository, bucket, metadata) = locked_version(state).await;

    let result = repository
        .delete_object(bucket.id, &metadata.key, NewDeleteMarker::generate())
        .await
        .expect("placing a delete marker stays allowed");
    assert!(result.delete_marker.is_some());

    // The retained version survives the marker and keeps its lock.
    assert!(
        repository
            .get_object_version(bucket.id, &metadata.key, metadata.version_id)
            .await
            .expect("read version")
            .is_some()
    );
    assert!(
        !repository
            .get_object_lock(metadata.version_id)
            .await
            .expect("read lock")
            .is_unlocked()
    );
}

#[tokio::test]
async fn overwriting_a_key_adds_a_version_and_leaves_the_locked_one_intact() {
    let state = ObjectLockState {
        retention: Some(Retention {
            mode: RetentionMode::Compliance,
            retain_until: Utc::now() + Duration::days(30),
        }),
        legal_hold: false,
    };
    let (_directory, repository, bucket, first) = locked_version(state).await;

    let mut second = object(bucket.id, "statement.pdf");
    second.size = 22;
    repository
        .put_object(&second, None, WriteOrigin::Direct)
        .await
        .expect("overwrite publishes a new version");

    let original = repository
        .get_object_version(bucket.id, &first.key, first.version_id)
        .await
        .expect("read original")
        .expect("the original version is still present");
    assert_eq!(original.version_id(), first.version_id);
    assert_eq!(
        repository
            .get_object_lock(first.version_id)
            .await
            .expect("read lock")
            .retention
            .expect("retention")
            .mode,
        RetentionMode::Compliance,
        "the locked version keeps its retention across an overwrite"
    );
}

#[tokio::test]
async fn a_compliance_retention_extends_but_never_shortens() {
    let retain_until = Utc::now() + Duration::days(30);
    let (_directory, repository, bucket, metadata) = locked_version(ObjectLockState {
        retention: Some(Retention {
            mode: RetentionMode::Compliance,
            retain_until,
        }),
        legal_hold: false,
    })
    .await;

    let extended = Retention {
        mode: RetentionMode::Compliance,
        retain_until: retain_until + Duration::days(1),
    };
    let updated = repository
        .put_object_lock(
            bucket.id,
            &metadata.key,
            metadata.version_id,
            ObjectLockState {
                retention: Some(extended),
                legal_hold: false,
            },
            release(),
        )
        .await
        .expect("extension is always allowed");
    assert_eq!(updated.retention, Some(extended));

    let shortened = Retention {
        mode: RetentionMode::Compliance,
        retain_until,
    };
    let error = repository
        .put_object_lock(
            bucket.id,
            &metadata.key,
            metadata.version_id,
            ObjectLockState {
                retention: Some(shortened),
                legal_hold: false,
            },
            release().with_governance_bypass(true),
        )
        .await
        .expect_err("a compliance retention never shortens");
    assert!(matches!(
        error,
        MetadataError::ObjectLockChangeRefused(LockChangeRefused::ComplianceRetentionIsFinal)
    ));
}

#[tokio::test]
async fn a_lock_cannot_be_placed_on_a_bucket_without_object_lock() {
    let (_directory, repository) = catalog().await;
    let mut bucket = locked_bucket("plain", None);
    bucket.object_lock = None;
    bucket.versioning = VersioningState::Enabled;
    repository.create_bucket(&bucket).await.expect("bucket");
    let metadata = object(bucket.id, "note.txt");
    repository
        .put_object(&metadata, None, WriteOrigin::Direct)
        .await
        .expect("put");

    let error = repository
        .put_object_lock(
            bucket.id,
            &metadata.key,
            metadata.version_id,
            ObjectLockState {
                retention: Some(Retention {
                    mode: RetentionMode::Governance,
                    retain_until: Utc::now() + Duration::days(1),
                }),
                legal_hold: false,
            },
            release(),
        )
        .await
        .expect_err("object lock is not enabled on this bucket");
    assert!(matches!(error, MetadataError::ObjectLockNotEnabled));
}

#[tokio::test]
async fn object_lock_requires_versioning_at_creation_and_keeps_it_afterwards() {
    let (_directory, repository) = catalog().await;

    let mut unversioned = locked_bucket("unversioned", None);
    unversioned.versioning = VersioningState::Disabled;
    let error = repository
        .create_bucket(&unversioned)
        .await
        .expect_err("object lock needs version history to protect");
    assert!(matches!(error, MetadataError::ObjectLockRequiresVersioning));

    let bucket = locked_bucket("records", None);
    repository.create_bucket(&bucket).await.expect("bucket");
    let error = repository
        .set_bucket_versioning(bucket.id, VersioningState::Suspended)
        .await
        .expect_err("suspending versioning would let a write replace a locked version");
    assert!(matches!(error, MetadataError::ObjectLockRequiresVersioning));
}

/// A retention date is worth exactly as much as the clock that judges it. Once
/// the catalog has observed a point in time, a clock that claims to be behind
/// it must not be allowed to release anything.
#[tokio::test]
async fn a_clock_behind_the_high_water_mark_refuses_to_release_a_retained_version() {
    let state = ObjectLockState {
        retention: Some(Retention {
            mode: RetentionMode::Governance,
            retain_until: Utc::now() + Duration::days(30),
        }),
        legal_hold: false,
    };
    let (_directory, repository, bucket, metadata) = locked_version(state).await;

    let now = Utc::now();
    repository
        .observe_clock(LockRelease::new(now, TOLERANCE_SECONDS))
        .await
        .expect("the mark advances to now");

    // A week earlier is well outside any NTP correction the tolerance absorbs.
    let rewound =
        LockRelease::new(now - Duration::days(7), TOLERANCE_SECONDS).with_governance_bypass(true);
    let error = repository
        .delete_object_version(bucket.id, &metadata.key, metadata.version_id, rewound)
        .await
        .expect_err("a rewound clock cannot release a retained version");
    assert!(
        matches!(error, MetadataError::ClockWentBackwards),
        "{error}"
    );

    // The same operation succeeds once the clock is trustworthy again.
    repository
        .delete_object_version(
            bucket.id,
            &metadata.key,
            metadata.version_id,
            release().with_governance_bypass(true),
        )
        .await
        .expect("a correct clock releases it")
        .expect("the version existed");
}

#[tokio::test]
async fn ordinary_clock_drift_inside_the_tolerance_is_absorbed() {
    let now = Utc::now();
    let (_directory, repository) = catalog().await;
    repository
        .observe_clock(LockRelease::new(now, TOLERANCE_SECONDS))
        .await
        .expect("mark");
    repository
        .observe_clock(LockRelease::new(
            now - Duration::seconds(i64::from(TOLERANCE_SECONDS) - 1),
            TOLERANCE_SECONDS,
        ))
        .await
        .expect("a correction inside the tolerance is not a backwards jump");
}

/// Adding protection can only ever over-retain, so it must keep working even
/// while the clock is not trusted. Refusing it would let a bad clock stop an
/// operator from protecting a record.
#[tokio::test]
async fn a_tightening_change_still_applies_while_the_clock_is_untrusted() {
    let (_directory, repository, bucket, metadata) =
        locked_version(ObjectLockState::default()).await;
    let now = Utc::now();
    repository
        .observe_clock(LockRelease::new(now, TOLERANCE_SECONDS))
        .await
        .expect("mark");

    let rewound = LockRelease::new(now - Duration::days(7), TOLERANCE_SECONDS);
    let updated = repository
        .put_object_lock(
            bucket.id,
            &metadata.key,
            metadata.version_id,
            ObjectLockState {
                retention: None,
                legal_hold: true,
            },
            rewound,
        )
        .await
        .expect("placing a hold adds protection and needs no trusted clock");
    assert!(updated.legal_hold);

    // Removing it is a release, so it is judged against the mark and refused.
    let error = repository
        .put_object_lock(
            bucket.id,
            &metadata.key,
            metadata.version_id,
            ObjectLockState::default(),
            rewound,
        )
        .await
        .expect_err("removing a hold is a release");
    assert!(
        matches!(error, MetadataError::ClockWentBackwards),
        "{error}"
    );
}

#[tokio::test]
async fn the_clock_mark_survives_a_restart() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("catalog.redb");
    let now = Utc::now();
    {
        let repository = RedbMetadataRepository::open(&path).await.expect("open");
        repository
            .observe_clock(LockRelease::new(now, TOLERANCE_SECONDS))
            .await
            .expect("mark");
    }
    let repository = RedbMetadataRepository::open(&path).await.expect("reopen");
    let error = repository
        .observe_clock(LockRelease::new(now - Duration::days(1), TOLERANCE_SECONDS))
        .await
        .expect_err("the mark is durable, so a restart does not reset it");
    assert!(
        matches!(error, MetadataError::ClockWentBackwards),
        "{error}"
    );
}

#[tokio::test]
async fn a_bucket_default_materializes_onto_versions_written_under_it() {
    let (_directory, repository) = catalog().await;
    let bucket = locked_bucket(
        "records",
        Some(DefaultRetention {
            mode: RetentionMode::Compliance,
            period: RetentionPeriod::Days(7),
        }),
    );
    repository.create_bucket(&bucket).await.expect("bucket");

    let stored = repository
        .get_bucket(bucket.id)
        .await
        .expect("read bucket")
        .expect("bucket exists");
    let configuration = stored.object_lock.expect("object lock is enabled");
    let state = configuration
        .initial_state_at(Utc::now())
        .expect("initial state");
    assert_eq!(
        state.retention.expect("retention").mode,
        RetentionMode::Compliance
    );
}

/// A member that catches up by installing a consensus snapshot must inherit
/// Object Lock state, not an empty lock table.
///
/// This is not a cosmetic gap. An absent lock record reads as "no lock", so a
/// member restored from a snapshot that omitted them would answer every
/// retention check with "deletable". The moment that member gained metadata
/// authority — an ordinary failover, or simply being the node a delete is
/// routed to — a compliance-retained version could be destroyed while the
/// cluster still reported the object as protected.
#[tokio::test]
async fn a_member_restored_from_a_snapshot_still_enforces_retention() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let leader = RedbMetadataRepository::open(directory.path().join("leader.redb"))
        .await
        .expect("catalog");
    let bucket = locked_bucket("compliance", None);
    leader.create_bucket(&bucket).await.expect("bucket");
    let metadata = object(bucket.id, "sealed");
    let retention = Retention {
        mode: RetentionMode::Compliance,
        retain_until: Utc::now() + Duration::days(30),
    };
    leader
        .put_object(
            &metadata,
            Some(ObjectLockState {
                retention: Some(retention),
                legal_hold: false,
            }),
            WriteOrigin::Direct,
        )
        .await
        .expect("put a retained version");

    // The leader builds a snapshot, exactly as it does for a follower that has
    // fallen behind the retained log.
    let source = leader.database();
    let entries = tokio::task::spawn_blocking(move || {
        use redb::ReadableDatabase as _;
        let read = source.begin_read().expect("begin");
        record_store_metadata::export_tx(&read).expect("export")
    })
    .await
    .expect("join");

    let follower = RedbMetadataRepository::open(directory.path().join("follower.redb"))
        .await
        .expect("catalog");
    let destination = follower.database();
    tokio::task::spawn_blocking(move || {
        let write = destination.begin_write().expect("begin");
        record_store_metadata::import_tx(&write, &entries).expect("import");
        write.commit().expect("commit");
    })
    .await
    .expect("join");

    let restored = follower
        .get_object_lock(metadata.version_id)
        .await
        .expect("read the restored lock");
    assert!(
        restored.retention.is_some(),
        "a snapshot must carry Object Lock state, or the restored member reports every \
         retained version as unlocked"
    );

    // The property that actually matters: the restored member refuses the delete.
    let refusal = follower
        .delete_object_version(bucket.id, &metadata.key, metadata.version_id, release())
        .await
        .expect_err("a compliance retention must still refuse the delete");
    assert!(
        matches!(refusal, MetadataError::VersionLocked(_)),
        "expected the restored member to refuse on Object Lock, got {refusal:?}"
    );
}

/// The clock high-water mark is what refuses a backwards clock. A snapshot that
/// dropped it would let a restored member accept a time the cluster has already
/// moved past, which is the window retention enforcement depends on being shut.
#[tokio::test]
async fn a_member_restored_from_a_snapshot_inherits_the_clock_high_water_mark() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let leader = RedbMetadataRepository::open(directory.path().join("leader.redb"))
        .await
        .expect("catalog");
    let far_ahead = Utc::now() + Duration::days(365);
    leader
        .observe_clock(LockRelease::new(far_ahead, TOLERANCE_SECONDS))
        .await
        .expect("advance the mark");

    let source = leader.database();
    let entries = tokio::task::spawn_blocking(move || {
        use redb::ReadableDatabase as _;
        let read = source.begin_read().expect("begin");
        record_store_metadata::export_tx(&read).expect("export")
    })
    .await
    .expect("join");

    let follower = RedbMetadataRepository::open(directory.path().join("follower.redb"))
        .await
        .expect("catalog");
    let destination = follower.database();
    tokio::task::spawn_blocking(move || {
        let write = destination.begin_write().expect("begin");
        record_store_metadata::import_tx(&write, &entries).expect("import");
        write.commit().expect("commit");
    })
    .await
    .expect("join");

    let rolled_back = follower
        .observe_clock(LockRelease::new(Utc::now(), TOLERANCE_SECONDS))
        .await;
    assert!(
        matches!(rolled_back, Err(MetadataError::ClockWentBackwards)),
        "the restored member must inherit the mark and refuse a clock behind it, got \
         {rolled_back:?}"
    );
}
