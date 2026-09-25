//! What a committed mutation owes its subscribers, and how that survives.
//!
//! The property under test is not "an event is published". It is that the
//! obligation to publish becomes durable *with* the mutation, so that a crash
//! anywhere afterwards leaves the event recoverable rather than lost. Every
//! test here therefore inspects the catalog's journal directly at least once,
//! rather than only checking that the outbox eventually filled up.
//!
//! The crashes simulated here are process-level: the drain is interrupted
//! between its two durable steps, and the components are dropped and reopened
//! from the same directory. They are not kills of a running server, which is
//! stated plainly because the two establish different things.

use std::{collections::BTreeMap, sync::Arc};

use bytes::Bytes;
use futures_util::stream;
use record_store_core::{BucketName, ObjectKey, OrganizationId, StorageEventType};
use record_store_events::{EventQuery, EventRepository, RedbEventRepository, WebhookConfig};
use record_store_metadata::{MetadataRepository, RedbMetadataRepository};
use record_store_service::{
    ObjectLockLimits, ServiceLimits, ServicePutRequest, Services, StorageEventPump,
};
use record_store_storage::{LocalFilesystemStore, ObjectStore, upload_stream};
use tempfile::TempDir;

struct Fixture {
    metadata: Arc<dyn MetadataRepository>,
    events: Arc<dyn EventRepository>,
    services: Services,
}

/// Opens the catalog, the store, and the outbox over one directory.
///
/// Taking a path rather than owning the directory is what lets a test drop
/// everything and open it again, which is the only way to exercise what a
/// restart actually recovers.
async fn open(path: &std::path::Path) -> Fixture {
    let metadata: Arc<dyn MetadataRepository> = Arc::new(
        RedbMetadataRepository::open(path.join("metadata.redb"))
            .await
            .expect("metadata repository"),
    );
    let storage: Arc<dyn ObjectStore> = Arc::new(
        LocalFilesystemStore::open(path, path.join("tmp"), Arc::clone(&metadata))
            .await
            .expect("filesystem store"),
    );
    let events: Arc<dyn EventRepository> = Arc::new(
        RedbEventRepository::open(path.join("events.redb"), None, WebhookConfig::default())
            .await
            .expect("event outbox"),
    );
    let services = Services::new(
        storage,
        Arc::clone(&metadata),
        OrganizationId::new(),
        ServiceLimits {
            maximum_concurrent_operations: 8,
            admission_wait_limit_seconds: 5,
            maximum_custom_metadata_entries: 8,
            maximum_custom_metadata_bytes: 1_024,
            object_lock: ObjectLockLimits::default(),
        },
    );
    Fixture {
        metadata,
        events,
        services,
    }
}

async fn fixture() -> (TempDir, Fixture) {
    let directory = tempfile::tempdir().expect("temporary directory");
    let fixture = open(directory.path()).await;
    (directory, fixture)
}

fn pump(fixture: &Fixture) -> StorageEventPump {
    StorageEventPump::new(
        Arc::clone(&fixture.metadata),
        Arc::clone(&fixture.events),
        std::time::Duration::from_secs(60),
    )
}

fn body(bytes: &'static [u8]) -> record_store_storage::UploadStream {
    upload_stream(stream::once(async move { Ok(Bytes::from_static(bytes)) }))
}

async fn put(services: &Services, bucket: &BucketName, key: &str, bytes: &'static [u8]) {
    services
        .objects
        .put(ServicePutRequest {
            bucket: bucket.clone(),
            key: ObjectKey::new(key).expect("key"),
            content_type: None,
            custom_metadata: BTreeMap::new(),
            expected_checksum: None,
            object_lock: None,
            body: body(bytes),
        })
        .await
        .expect("put");
}

async fn delivered(fixture: &Fixture) -> Vec<StorageEventType> {
    let mut events = fixture
        .events
        .list_events(EventQuery {
            limit: 100,
            ..EventQuery::default()
        })
        .await
        .expect("list events")
        .events;
    events.reverse();
    events.into_iter().map(|event| event.event_type).collect()
}

/// The obligation is durable before anything has been published. This is the
/// whole guarantee: a crash here loses the event under the old arrangement and
/// loses nothing under this one.
#[tokio::test]
async fn a_committed_mutation_leaves_its_event_in_the_catalog_before_anything_is_published() {
    let (_directory, fixture) = fixture().await;
    let bucket = BucketName::new("journalled").expect("bucket");
    fixture
        .services
        .buckets
        .create(bucket.clone())
        .await
        .expect("create bucket");
    put(&fixture.services, &bucket, "a.txt", b"hello").await;

    let journalled = fixture
        .metadata
        .pending_mutation_events(0, 100)
        .await
        .expect("journal");
    assert_eq!(
        journalled
            .iter()
            .map(|event| event.event_type)
            .collect::<Vec<_>>(),
        vec![
            StorageEventType::BucketCreated,
            StorageEventType::ObjectCreated
        ]
    );
    assert_eq!(journalled[0].bucket, "journalled");
    assert_eq!(journalled[1].key.as_deref(), Some("a.txt"));
    assert_eq!(journalled[1].size, Some(5));
    assert!(
        journalled[0].sequence < journalled[1].sequence,
        "journal order is commit order"
    );

    // Nothing has reached the outbox yet, which is the point: the catalog is
    // the authority on what is owed, and delivery is a separate step.
    assert!(delivered(&fixture).await.is_empty());
}

/// Draining moves what is owed into the outbox and clears the journal, so the
/// journal does not grow without bound on a busy deployment.
#[tokio::test]
async fn draining_publishes_every_owed_event_once_and_clears_the_journal() {
    let (_directory, fixture) = fixture().await;
    let bucket = BucketName::new("drained").expect("bucket");
    fixture
        .services
        .buckets
        .create(bucket.clone())
        .await
        .expect("create bucket");
    put(&fixture.services, &bucket, "a.txt", b"one").await;
    put(&fixture.services, &bucket, "a.txt", b"two").await;
    fixture
        .services
        .objects
        .delete(&bucket, ObjectKey::new("a.txt").expect("key"))
        .await
        .expect("delete");

    let pump = pump(&fixture);
    assert_eq!(pump.run_once().await.expect("drain"), 4);
    assert_eq!(
        delivered(&fixture).await,
        vec![
            StorageEventType::BucketCreated,
            StorageEventType::ObjectCreated,
            StorageEventType::ObjectUpdated,
            StorageEventType::ObjectDeleted,
        ]
    );
    assert!(
        fixture
            .metadata
            .pending_mutation_events(0, 100)
            .await
            .expect("journal")
            .is_empty(),
        "drained rows must not stay in the catalog"
    );

    // Running again publishes nothing: the outbox remembers how far it drained.
    assert_eq!(pump.run_once().await.expect("second drain"), 0);
    assert_eq!(delivered(&fixture).await.len(), 4);
}

/// The drain's two durable steps are in two different databases. The outbox
/// commit is the one that counts, and it records how far it has taken in the
/// same transaction, so an interruption before the catalog is pruned cannot
/// publish anything twice.
#[tokio::test]
async fn an_interruption_between_the_outbox_commit_and_the_prune_publishes_nothing_twice() {
    let (_directory, fixture) = fixture().await;
    let bucket = BucketName::new("interrupted").expect("bucket");
    fixture
        .services
        .buckets
        .create(bucket.clone())
        .await
        .expect("create bucket");
    put(&fixture.services, &bucket, "a.txt", b"one").await;

    let journalled = fixture
        .metadata
        .pending_mutation_events(0, 100)
        .await
        .expect("journal");
    assert_eq!(journalled.len(), 2);

    // Take them into the outbox, then stop — exactly as a crash between the
    // two steps would leave things. The journal still holds every row.
    let through = fixture
        .events
        .drain_journal(&journalled)
        .await
        .expect("drain");
    assert_eq!(through, journalled[1].sequence);
    assert_eq!(
        fixture
            .metadata
            .pending_mutation_events(0, 100)
            .await
            .expect("journal")
            .len(),
        2,
        "the catalog was never told, so the rows are still there"
    );

    // Replaying the same rows is what recovery does. It must not republish.
    let replayed = fixture
        .events
        .drain_journal(&journalled)
        .await
        .expect("replay");
    assert_eq!(replayed, through);
    assert_eq!(delivered(&fixture).await.len(), 2, "no duplicates");

    // And a normal pass afterwards clears the journal without publishing more.
    assert_eq!(pump(&fixture).run_once().await.expect("pass"), 0);
    assert!(
        fixture
            .metadata
            .pending_mutation_events(0, 100)
            .await
            .expect("journal")
            .is_empty()
    );
    assert_eq!(delivered(&fixture).await.len(), 2);
}

/// A restart with an undrained journal delivers what the previous process
/// committed but never published.
#[tokio::test]
async fn a_restart_delivers_events_the_previous_process_never_published() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().to_path_buf();
    {
        let fixture = open(&path).await;
        let bucket = BucketName::new("recovered").expect("bucket");
        fixture
            .services
            .buckets
            .create(bucket.clone())
            .await
            .expect("create bucket");
        put(&fixture.services, &bucket, "a.txt", b"survives").await;
        // Deliberately no drain: this process dies owing two events.
        assert!(delivered(&fixture).await.is_empty());
    }

    // Reopen the same directory, as a restarted process would.
    let restarted = open(&path).await;
    let events = Arc::clone(&restarted.events);
    let pump = pump(&restarted);
    assert_eq!(
        pump.run_once().await.expect("drain after restart"),
        2,
        "the events the previous process owed must still be owed"
    );
    let names: Vec<_> = events
        .list_events(EventQuery {
            limit: 100,
            ..EventQuery::default()
        })
        .await
        .expect("list")
        .events
        .into_iter()
        .map(|event| event.event_type)
        .collect();
    assert!(
        names.contains(&StorageEventType::BucketCreated),
        "{names:?}"
    );
    assert!(
        names.contains(&StorageEventType::ObjectCreated),
        "{names:?}"
    );
}

/// The identifier is allocated when the mutation commits, not when the event
/// is published, so a republish after a crash is recognisably the same event
/// rather than a second one a subscriber cannot deduplicate.
#[tokio::test]
async fn an_event_keeps_the_identifier_it_was_committed_with() {
    let (_directory, fixture) = fixture().await;
    let bucket = BucketName::new("stable-ids").expect("bucket");
    fixture
        .services
        .buckets
        .create(bucket.clone())
        .await
        .expect("create bucket");

    let journalled = fixture
        .metadata
        .pending_mutation_events(0, 100)
        .await
        .expect("journal");
    let committed_id = journalled[0].event_id;
    pump(&fixture).run_once().await.expect("drain");

    let published = fixture
        .events
        .list_events(EventQuery {
            limit: 10,
            ..EventQuery::default()
        })
        .await
        .expect("list")
        .events;
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].id, committed_id);
}

/// A copy, a restore, and a completed multipart upload all publish a version
/// and are indistinguishable in the catalog. The origin is what keeps the
/// events a subscriber receives describing what actually happened.
#[tokio::test]
async fn the_event_names_what_happened_rather_than_only_that_a_version_appeared() {
    let (_directory, fixture) = fixture().await;
    let bucket = BucketName::new("origins").expect("bucket");
    fixture
        .services
        .buckets
        .create(bucket.clone())
        .await
        .expect("create bucket");
    fixture
        .services
        .buckets
        .set_versioning(&bucket, record_store_core::VersioningState::Enabled)
        .await
        .expect("enable versioning");
    put(&fixture.services, &bucket, "a.txt", b"first").await;
    let first = fixture
        .services
        .objects
        .head(&bucket, ObjectKey::new("a.txt").expect("key"))
        .await
        .expect("head")
        .version_id;
    put(&fixture.services, &bucket, "a.txt", b"second").await;

    fixture
        .services
        .objects
        .restore_version(&bucket, ObjectKey::new("a.txt").expect("key"), first)
        .await
        .expect("restore");

    pump(&fixture).run_once().await.expect("drain");
    let names = delivered(&fixture).await;
    assert!(
        names.contains(&StorageEventType::ObjectRestored),
        "a restore must be reported as one: {names:?}"
    );
    assert!(
        names.contains(&StorageEventType::ObjectUpdated),
        "and it is also a new current version: {names:?}"
    );
}

/// Aborting a multipart upload is its own event; completing one is reported as
/// a completion rather than as an ordinary upload.
#[tokio::test]
async fn multipart_completion_and_abort_keep_their_own_event_names() {
    let (_directory, fixture) = fixture().await;
    let bucket = BucketName::new("multipart-events").expect("bucket");
    fixture
        .services
        .buckets
        .create(bucket.clone())
        .await
        .expect("create bucket");

    let upload = fixture
        .services
        .objects
        .create_multipart(record_store_service::ServiceCreateMultipartRequest {
            bucket: bucket.clone(),
            key: ObjectKey::new("big.bin").expect("key"),
            content_type: None,
            custom_metadata: BTreeMap::new(),
            object_lock: None,
        })
        .await
        .expect("create upload");
    let part = fixture
        .services
        .objects
        .upload_part(record_store_service::ServiceUploadPartRequest {
            bucket: bucket.clone(),
            key: ObjectKey::new("big.bin").expect("key"),
            upload_id: upload.id,
            number: record_store_core::PartNumber::new(1).expect("part number"),
            expected_checksum: None,
            body: body(b"assembled"),
        })
        .await
        .expect("upload part");
    fixture
        .services
        .objects
        .complete_multipart(record_store_service::ServiceCompleteMultipartRequest {
            bucket: bucket.clone(),
            key: ObjectKey::new("big.bin").expect("key"),
            upload_id: upload.id,
            manifest: vec![record_store_core::CompletedPart {
                number: part.number,
                etag: part.etag,
            }],
        })
        .await
        .expect("complete");

    let aborted = fixture
        .services
        .objects
        .create_multipart(record_store_service::ServiceCreateMultipartRequest {
            bucket: bucket.clone(),
            key: ObjectKey::new("abandoned.bin").expect("key"),
            content_type: None,
            custom_metadata: BTreeMap::new(),
            object_lock: None,
        })
        .await
        .expect("create upload");
    fixture
        .services
        .objects
        .abort_multipart(
            &bucket,
            &ObjectKey::new("abandoned.bin").expect("key"),
            aborted.id,
        )
        .await
        .expect("abort");

    pump(&fixture).run_once().await.expect("drain");
    let names = delivered(&fixture).await;
    assert!(
        names.contains(&StorageEventType::MultipartCompleted),
        "{names:?}"
    );
    assert!(
        names.contains(&StorageEventType::MultipartAborted),
        "{names:?}"
    );
    assert!(
        !names.contains(&StorageEventType::ObjectCreated),
        "a completion is not additionally an ordinary upload: {names:?}"
    );
}

/// A delete against a key that was never there changes nothing, so it owes
/// nothing. An event for it would tell subscribers about a mutation that did
/// not happen.
#[tokio::test]
async fn a_delete_that_changed_nothing_owes_no_event() {
    let (_directory, fixture) = fixture().await;
    let bucket = BucketName::new("no-op-delete").expect("bucket");
    fixture
        .services
        .buckets
        .create(bucket.clone())
        .await
        .expect("create bucket");
    let removed = fixture
        .services
        .objects
        .delete(&bucket, ObjectKey::new("absent.txt").expect("key"))
        .await
        .expect("delete");
    assert!(!removed);

    let journalled = fixture
        .metadata
        .pending_mutation_events(0, 100)
        .await
        .expect("journal");
    assert_eq!(
        journalled
            .iter()
            .map(|event| event.event_type)
            .collect::<Vec<_>>(),
        vec![StorageEventType::BucketCreated]
    );
}

/// An outbox that fails on demand, so the drain can be made to break where a
/// full disk would break it.
struct FaultyOutbox {
    inner: Arc<dyn EventRepository>,
    failing: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl EventRepository for FaultyOutbox {
    async fn publish(
        &self,
        event: &record_store_events::StorageEvent,
    ) -> Result<(), record_store_events::EventError> {
        self.inner.publish(event).await
    }

    async fn drained_through(&self) -> Result<u64, record_store_events::EventError> {
        self.inner.drained_through().await
    }

    async fn drain_journal(
        &self,
        events: &[record_store_core::MutationEvent],
    ) -> Result<u64, record_store_events::EventError> {
        if self.failing.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(record_store_events::EventError::Database(
                "injected failure".into(),
            ));
        }
        self.inner.drain_journal(events).await
    }

    async fn create_webhook(
        &self,
        request: record_store_events::CreateWebhookRequest,
    ) -> Result<record_store_events::CreatedWebhook, record_store_events::EventError> {
        self.inner.create_webhook(request).await
    }

    async fn list_webhooks(
        &self,
    ) -> Result<Vec<record_store_events::WebhookSubscription>, record_store_events::EventError>
    {
        self.inner.list_webhooks().await
    }

    async fn set_webhook_enabled(
        &self,
        id: record_store_core::WebhookId,
        enabled: bool,
    ) -> Result<record_store_events::WebhookSubscription, record_store_events::EventError> {
        self.inner.set_webhook_enabled(id, enabled).await
    }

    async fn delete_webhook(
        &self,
        id: record_store_core::WebhookId,
    ) -> Result<(), record_store_events::EventError> {
        self.inner.delete_webhook(id).await
    }

    async fn list_delivery_logs(
        &self,
        limit: usize,
    ) -> Result<Vec<record_store_events::WebhookDeliveryLog>, record_store_events::EventError> {
        self.inner.list_delivery_logs(limit).await
    }

    async fn list_events(
        &self,
        query: EventQuery,
    ) -> Result<record_store_events::EventPage, record_store_events::EventError> {
        self.inner.list_events(query).await
    }

    async fn deliver_due(&self, limit: usize) -> Result<usize, record_store_events::EventError> {
        self.inner.deliver_due(limit).await
    }

    async fn check_ready(&self) -> Result<(), record_store_events::EventError> {
        self.inner.check_ready().await
    }
}

/// An outbox that cannot be written keeps the obligation where it is. The
/// mutation is not rolled back — it is committed and correct — but the event
/// it owes stays in the catalog until the outbox can take it.
#[tokio::test]
async fn an_outbox_that_cannot_be_written_keeps_the_event_owed() {
    let (_directory, fixture) = fixture().await;
    let bucket = BucketName::new("outbox-broken").expect("bucket");
    fixture
        .services
        .buckets
        .create(bucket.clone())
        .await
        .expect("create bucket");
    put(&fixture.services, &bucket, "a.txt", b"owed").await;

    let faulty: Arc<dyn EventRepository> = Arc::new(FaultyOutbox {
        inner: Arc::clone(&fixture.events),
        failing: std::sync::atomic::AtomicBool::new(true),
    });
    let broken = StorageEventPump::new(
        Arc::clone(&fixture.metadata),
        Arc::clone(&faulty),
        std::time::Duration::from_secs(60),
    );
    assert!(
        broken.run_once().await.is_err(),
        "a drain that cannot write must report the failure"
    );
    assert_eq!(
        fixture
            .metadata
            .pending_mutation_events(0, 100)
            .await
            .expect("journal")
            .len(),
        2,
        "the obligation stays in the catalog until it is discharged"
    );
    assert!(
        delivered(&fixture).await.is_empty(),
        "and nothing was published"
    );

    // The object is there regardless: the mutation was never in doubt.
    fixture
        .services
        .objects
        .head(&bucket, ObjectKey::new("a.txt").expect("key"))
        .await
        .expect("the committed object is unaffected");

    // Once the outbox recovers, the owed events are published — the same two,
    // in commit order, not a replacement pair.
    assert_eq!(pump(&fixture).run_once().await.expect("recovered"), 2);
    assert_eq!(
        delivered(&fixture).await,
        vec![
            StorageEventType::BucketCreated,
            StorageEventType::ObjectCreated
        ]
    );
}
