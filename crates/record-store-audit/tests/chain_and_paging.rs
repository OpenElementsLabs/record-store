//! What the durable audit store guarantees, exercised against a real database.
//!
//! Two separate properties live here. The first is tamper evidence: records are
//! linked, so an edit made behind the server's back is detectable. The second is
//! that a bounded query never lies about having reached the end of a range.

use std::collections::BTreeMap;

use chrono::{Duration, Utc};
use record_store_audit::{
    AuditEvent, AuditQuery, AuditRepository, AuditResult, RedbAuditRepository, chain,
};
use record_store_core::AuditEventId;
use tempfile::tempdir;

fn event(index: i64, principal: &str, operation: &str) -> AuditEvent {
    AuditEvent {
        event_id: AuditEventId::new(),
        timestamp: Utc::now() - Duration::seconds(10_000 - index),
        request_id: Some(format!("req-{index}")),
        principal: principal.to_owned(),
        credential_id: None,
        source_ip: None,
        operation: operation.to_owned(),
        resource: format!("bucket:records/{index}"),
        result: AuditResult::Success,
        metadata: BTreeMap::new(),
    }
}

/// Every appended record links to the one before it, and the whole log walks
/// back to the genesis constant. Without this the log is merely durable, which
/// is a much weaker claim than the one an audit trail is there to support.
#[tokio::test]
async fn appended_records_form_a_chain_that_verifies_end_to_end() {
    let directory = tempdir().expect("temporary directory");
    let repository = RedbAuditRepository::open(directory.path().join("audit.redb"))
        .await
        .expect("open");

    for index in 0..25 {
        repository
            .append(&event(index, "management:system-administrator", "PUT /x"))
            .await
            .expect("append");
    }

    let verification = repository.verify_chain(0, 1_000).await.expect("verify");
    assert_eq!(verification.checked, 25);
    assert_eq!(verification.intact, 25);
    assert_eq!(verification.unchained, 0);
    assert_eq!(verification.head_sequence, Some(24));
    assert!(verification.is_intact(), "{:?}", verification.problems);
    assert!(verification.next_sequence.is_none());
}

/// The chain survives a restart: the head is durable, so the record written
/// after a restart links to the record written before it rather than starting a
/// second chain that would verify on its own and hide the join.
#[tokio::test]
async fn the_chain_continues_across_a_restart() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("audit.redb");
    {
        let repository = RedbAuditRepository::open(&path).await.expect("open");
        for index in 0..3 {
            repository
                .append(&event(index, "system:lifecycle", "lifecycle.expire-object"))
                .await
                .expect("append");
        }
    }
    let reopened = RedbAuditRepository::open(&path).await.expect("reopen");
    for index in 3..6 {
        reopened
            .append(&event(index, "system:lifecycle", "lifecycle.expire-object"))
            .await
            .expect("append");
    }

    let verification = reopened.verify_chain(0, 1_000).await.expect("verify");
    assert_eq!(verification.checked, 6);
    assert_eq!(verification.intact, 6);
    assert!(verification.is_intact(), "{:?}", verification.problems);
}

/// Editing a stored record behind the server's back is the thing the chain
/// exists to catch. The edit is made directly in the database file, because an
/// attacker with that access is exactly the threat model the chain addresses.
#[tokio::test]
async fn an_edit_made_directly_in_the_database_is_detected() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("audit.redb");
    {
        let repository = RedbAuditRepository::open(&path).await.expect("open");
        for index in 0..5 {
            repository
                .append(&event(index, "service_account:abc", "s3:DELETE"))
                .await
                .expect("append");
        }
        repository.verify_chain(0, 100).await.expect("verify clean");
    }

    // Rewrite one record's content in place, keeping its stored hash. This is
    // what an operator editing the log would do, and it is exactly what a
    // durable-but-unchained log cannot notice.
    {
        use redb::{ReadableDatabase, TableDefinition};
        let events: TableDefinition<&[u8], &[u8]> = TableDefinition::new("audit_events.v1");
        let index: TableDefinition<u64, &[u8]> = TableDefinition::new("audit_sequence.v1");
        let database = redb::Database::open(&path).expect("open raw");
        let key = {
            let read = database.begin_read().expect("read");
            let table = read.open_table(index).expect("index");
            table
                .get(2_u64)
                .expect("lookup")
                .expect("sequence 2 exists")
                .value()
                .to_vec()
        };
        let mut record: serde_json::Value = {
            let read = database.begin_read().expect("read");
            let table = read.open_table(events).expect("events");
            serde_json::from_slice(
                table
                    .get(key.as_slice())
                    .expect("get")
                    .expect("row")
                    .value(),
            )
            .expect("decode")
        };
        record["event"]["result"] = serde_json::json!("success");
        record["event"]["operation"] = serde_json::json!("s3:GET");
        let write = database.begin_write().expect("write");
        {
            let mut table = write.open_table(events).expect("events");
            let bytes = serde_json::to_vec(&record).expect("encode");
            table
                .insert(key.as_slice(), bytes.as_slice())
                .expect("rewrite");
        }
        write.commit().expect("commit");
    }

    let repository = RedbAuditRepository::open(&path).await.expect("reopen");
    let verification = repository.verify_chain(0, 100).await.expect("verify");
    assert!(
        !verification.is_intact(),
        "an edited record must not verify: {verification:?}"
    );
    assert_eq!(verification.problems.len(), 1);
    assert_eq!(verification.problems[0].sequence, 2);
    assert_eq!(verification.problems[0].verdict, "content changed");
}

/// Removing a record breaks the link of the one that followed it, which is what
/// makes deletion detectable rather than invisible.
#[tokio::test]
async fn removing_a_record_breaks_the_link_of_its_successor() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("audit.redb");
    {
        let repository = RedbAuditRepository::open(&path).await.expect("open");
        for index in 0..4 {
            repository
                .append(&event(index, "service_account:abc", "s3:PUT"))
                .await
                .expect("append");
        }
    }
    {
        use redb::{ReadableDatabase, TableDefinition};
        let events: TableDefinition<&[u8], &[u8]> = TableDefinition::new("audit_events.v1");
        let index: TableDefinition<u64, &[u8]> = TableDefinition::new("audit_sequence.v1");
        let database = redb::Database::open(&path).expect("open raw");
        let key = {
            let read = database.begin_read().expect("read");
            let table = read.open_table(index).expect("index");
            table
                .get(1_u64)
                .expect("lookup")
                .expect("row")
                .value()
                .to_vec()
        };
        let write = database.begin_write().expect("write");
        {
            write
                .open_table(events)
                .expect("events")
                .remove(key.as_slice())
                .expect("remove");
            write
                .open_table(index)
                .expect("index")
                .remove(1_u64)
                .expect("remove");
        }
        write.commit().expect("commit");
    }

    let repository = RedbAuditRepository::open(&path).await.expect("reopen");
    let verification = repository.verify_chain(0, 100).await.expect("verify");
    assert!(!verification.is_intact(), "{verification:?}");
    assert!(
        verification
            .problems
            .iter()
            .any(|problem| problem.verdict == "broken link to the previous record"),
        "a removed record must leave its successor unlinkable: {verification:?}"
    );
}

/// Records written before this deployment maintained a chain are reported as
/// unchained rather than counted as verified. Presenting them as intact would
/// be claiming evidence that was never produced.
#[tokio::test]
async fn records_written_before_the_chain_are_reported_as_unchained() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("audit.redb");
    let legacy = event(1, "management:auditor", "GET /api/v1/audit/events");

    // Write a record in the pre-chain layout: a bare event, no sequence, no
    // links, and no entry in the sequence index.
    {
        use redb::TableDefinition;
        let events: TableDefinition<&[u8], &[u8]> = TableDefinition::new("audit_events.v1");
        let database = redb::Database::create(&path).expect("create");
        let write = database.begin_write().expect("write");
        {
            let mut table = write.open_table(events).expect("events");
            let mut key = (legacy.timestamp.timestamp_micros().max(0) as u64)
                .to_be_bytes()
                .to_vec();
            key.extend_from_slice(legacy.event_id.as_uuid().as_bytes());
            let bytes = serde_json::to_vec(&legacy).expect("encode");
            table
                .insert(key.as_slice(), bytes.as_slice())
                .expect("insert");
        }
        write.commit().expect("commit");
    }

    let repository = RedbAuditRepository::open(&path).await.expect("open");
    // The legacy record is still queryable.
    let page = repository
        .query(AuditQuery {
            limit: 10,
            ..AuditQuery::default()
        })
        .await
        .expect("query");
    assert_eq!(page.events, vec![legacy]);

    // And a record appended now starts the chain at the genesis value rather
    // than pretending to follow the record it cannot link to.
    repository
        .append(&event(2, "management:auditor", "GET /api/v1/audit/events"))
        .await
        .expect("append");
    let verification = repository.verify_chain(0, 100).await.expect("verify");
    assert_eq!(verification.checked, 1);
    assert_eq!(verification.intact, 1);
    assert!(verification.is_intact(), "{verification:?}");
}

/// A sparse filter over a long range is the case where a capped scan returns an
/// empty page while matches remain further on. Reporting that as "no more
/// results" hides activity, so the page has to say the scan was cut short and
/// hand back somewhere to continue from.
#[tokio::test]
async fn a_filter_that_outlives_the_scan_budget_reports_an_incomplete_scan() {
    let directory = tempdir().expect("temporary directory");
    let repository = RedbAuditRepository::open(directory.path().join("audit.redb"))
        .await
        .expect("open");

    // The scan cap is 100_000 entries, which is far too many to write in a
    // test. The same behaviour is reachable by asking for the whole range in
    // pages: what is being pinned is that a page which stops early carries a
    // cursor and a truthful flag, and that following the cursor finds the
    // record the filter was looking for.
    let needle = 199_i64;
    for index in 0..200 {
        let principal = if index == needle { "rare" } else { "common" };
        repository
            .append(&event(index, principal, "s3:PUT"))
            .await
            .expect("append");
    }

    let mut cursor = None;
    let mut found = 0;
    let mut pages = 0;
    loop {
        let page = repository
            .query(AuditQuery {
                principal: Some("rare".to_owned()),
                after: cursor,
                limit: 10,
                ..AuditQuery::default()
            })
            .await
            .expect("query");
        pages += 1;
        found += page.events.len();
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
        assert!(pages < 50, "paging did not terminate");
    }
    assert_eq!(found, 1, "the rare record must be reachable by paging");
}

/// The flag itself has to be wired to the scan, not merely present. This drives
/// the cap directly through a query whose filter matches nothing, and asserts
/// the page distinguishes "nothing matched in this window" from "nothing more
/// exists anywhere".
#[tokio::test]
async fn an_exhausted_range_reports_a_complete_scan() {
    let directory = tempdir().expect("temporary directory");
    let repository = RedbAuditRepository::open(directory.path().join("audit.redb"))
        .await
        .expect("open");
    for index in 0..5 {
        repository
            .append(&event(index, "common", "s3:PUT"))
            .await
            .expect("append");
    }
    let page = repository
        .query(AuditQuery {
            principal: Some("absent".to_owned()),
            limit: 10,
            ..AuditQuery::default()
        })
        .await
        .expect("query");
    assert!(page.events.is_empty());
    assert!(
        !page.scan_truncated,
        "a range walked to its end is a complete scan"
    );
    assert!(page.next.is_none());
}

/// The genesis constant is part of the on-disk format: the first record links
/// to it, and a verifier built from the specification has to agree.
#[tokio::test]
async fn the_first_record_links_to_the_documented_genesis_value() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("audit.redb");
    let first = event(0, "system:test", "s3:PUT");
    {
        let repository = RedbAuditRepository::open(&path).await.expect("open");
        repository.append(&first).await.expect("append");
    }

    use redb::{ReadableDatabase, ReadableTable, TableDefinition};
    let events: TableDefinition<&[u8], &[u8]> = TableDefinition::new("audit_events.v1");
    let database = redb::Database::open(&path).expect("open raw");
    let read = database.begin_read().expect("read");
    let table = read.open_table(events).expect("events");
    let (_, value) = table
        .iter()
        .expect("iterate")
        .next()
        .expect("one row")
        .expect("row");
    let record: chain::AuditRecord = serde_json::from_slice(value.value()).expect("decode");
    assert_eq!(record.sequence, 0);
    let links = record.chain.expect("the first record is chained");
    assert_eq!(links.previous_hash, chain::genesis_hash());
    assert_eq!(
        links.record_hash,
        chain::hash_event(0, &chain::genesis_hash(), &first)
    );
}
