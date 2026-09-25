//! Streaming an audit range out, across more pages than one query returns.
//!
//! The properties worth defending are that every record appears exactly once in
//! order, that the JSON form is a single valid document rather than a stream of
//! fragments, and that none of it depends on holding the range.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{Duration, Utc};
use futures_util::StreamExt;
use record_store_audit::export::{
    EXPORT_PAGE_SIZE, ExportFormat, ExportRange, csv_header, export_stream,
};
use record_store_audit::{AuditEvent, AuditRepository, AuditResult, RedbAuditRepository};
use record_store_core::AuditEventId;

/// Writes `count` events one second apart, oldest first, and returns the base
/// instant so a caller can name range bounds that line up with them exactly.
async fn store_with(
    count: usize,
) -> (
    tempfile::TempDir,
    Arc<dyn AuditRepository>,
    chrono::DateTime<Utc>,
) {
    let directory = tempfile::tempdir().expect("temporary directory");
    let repository = Arc::new(
        RedbAuditRepository::open(directory.path().join("audit.redb"))
            .await
            .expect("open audit"),
    );
    let base = Utc::now() - Duration::days(1);
    for index in 0..count {
        repository
            .append(&AuditEvent {
                event_id: AuditEventId::new(),
                timestamp: base + Duration::seconds(index as i64),
                request_id: Some(format!("req-{index}")),
                principal: "service_account:exporter".into(),
                credential_id: None,
                source_ip: Some("203.0.113.7".into()),
                operation: "s3:PUT".into(),
                // Deliberately awkward for CSV: a comma, a quote and a newline.
                resource: format!("bucket:records/file {index},\"odd\"\nname"),
                result: AuditResult::Success,
                metadata: BTreeMap::from([("index".to_owned(), index.to_string())]),
            })
            .await
            .expect("append");
    }
    (directory, repository, base)
}

/// Collects a stream into bytes, the way a client writing to disk would.
async fn collect(
    repository: Arc<dyn AuditRepository>,
    format: ExportFormat,
    page_size: usize,
) -> Vec<u8> {
    let range = ExportRange::new(
        Utc::now() - Duration::days(2),
        Utc::now() + Duration::days(1),
    )
    .expect("range");
    let mut stream = Box::pin(export_stream(repository, range, format, page_size));
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        out.extend_from_slice(&chunk.expect("chunk"));
    }
    out
}

/// The export spans many pages, so this is where an off-by-one in the cursor
/// would show up as a duplicate or a missing record.
#[tokio::test]
async fn a_json_export_is_one_valid_document_covering_every_record_once() {
    let (_directory, repository, _base) = store_with(250).await;
    let bytes = collect(Arc::clone(&repository), ExportFormat::Json, 10).await;

    let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("a single JSON document");
    let array = parsed.as_array().expect("a JSON array");
    assert_eq!(array.len(), 250);

    let indexes: Vec<i64> = array
        .iter()
        .map(|event| {
            event["metadata"]["index"]
                .as_str()
                .expect("index")
                .parse()
                .expect("numeric")
        })
        .collect();
    let expected: Vec<i64> = (0..250).collect();
    assert_eq!(indexes, expected, "records must appear once, in order");
}

#[tokio::test]
async fn a_csv_export_round_trips_through_a_csv_reader_with_awkward_fields_intact() {
    let (_directory, repository, _base) = store_with(120).await;
    let bytes = collect(Arc::clone(&repository), ExportFormat::Csv, 7).await;
    let text = String::from_utf8(bytes).expect("utf-8");

    assert!(text.starts_with(csv_header()), "the header comes first");

    // A minimal RFC 4180 reader, so the test does not simply agree with the
    // writer about how quoting works.
    let rows = parse_csv(&text);
    assert_eq!(rows.len(), 121, "header plus one row per record");
    assert_eq!(rows[0].len(), 10);
    for (index, row) in rows[1..].iter().enumerate() {
        assert_eq!(row.len(), 10, "row {index}");
        assert_eq!(row[2], "service_account:exporter");
        assert_eq!(row[8], "success");
        assert_eq!(
            row[7],
            format!("bucket:records/file {index},\"odd\"\nname"),
            "an awkward field must survive quoting"
        );
        let metadata: serde_json::Value = serde_json::from_str(&row[9]).expect("metadata json");
        assert_eq!(metadata["index"], index.to_string());
    }
}

/// An empty range must still produce a well-formed document rather than
/// nothing, so a consumer can tell "no records" from "the export broke".
#[tokio::test]
async fn an_empty_range_still_produces_a_valid_document() {
    let (_directory, repository, _base) = store_with(0).await;

    let json = collect(Arc::clone(&repository), ExportFormat::Json, 10).await;
    let parsed: serde_json::Value = serde_json::from_slice(&json).expect("valid JSON");
    assert_eq!(parsed.as_array().expect("array").len(), 0);

    let csv = collect(repository, ExportFormat::Csv, 10).await;
    assert_eq!(String::from_utf8(csv).expect("utf-8"), csv_header());
}

/// The page size changes only how often the export goes back for more, never
/// what comes out.
#[tokio::test]
async fn the_page_size_does_not_change_the_output() {
    let (_directory, repository, _base) = store_with(97).await;
    let reference = collect(Arc::clone(&repository), ExportFormat::Json, 1).await;
    for page_size in [2, 5, 50, EXPORT_PAGE_SIZE, 10_000] {
        let other = collect(Arc::clone(&repository), ExportFormat::Json, page_size).await;
        assert_eq!(other, reference, "page size {page_size}");
    }
}

/// The range bounds are honoured, so an auditor asking for one day does not
/// receive a neighbouring one.
#[tokio::test]
async fn only_records_inside_the_range_are_exported() {
    let (_directory, repository, base) = store_with(100).await;
    let range = ExportRange::new(base + Duration::seconds(10), base + Duration::seconds(20))
        .expect("range");
    let mut stream = Box::pin(export_stream(repository, range, ExportFormat::Json, 4));
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        bytes.extend_from_slice(&chunk.expect("chunk"));
    }
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("valid JSON");
    let array = parsed.as_array().expect("array");
    assert!(!array.is_empty(), "the range holds records");
    for event in array {
        let index: i64 = event["metadata"]["index"]
            .as_str()
            .expect("index")
            .parse()
            .expect("numeric");
        assert!(
            (10..20).contains(&index),
            "record {index} is outside the range"
        );
    }
}

/// The export yields many chunks rather than one, which is what lets a caller
/// write to disk as it goes instead of buffering the range.
#[tokio::test]
async fn the_export_arrives_in_chunks_rather_than_all_at_once() {
    let (_directory, repository, _base) = store_with(300).await;
    let range = ExportRange::new(
        Utc::now() - Duration::days(2),
        Utc::now() + Duration::days(1),
    )
    .expect("range");
    let mut stream = Box::pin(export_stream(repository, range, ExportFormat::Json, 10));
    let mut chunks = 0_usize;
    while let Some(chunk) = stream.next().await {
        chunk.expect("chunk");
        chunks += 1;
    }
    assert!(chunks > 10, "expected many chunks, saw {chunks}");
}

/// A deliberately small RFC 4180 reader, independent of the writer.
fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match (quoted, character) {
            (true, '"') => {
                if characters.peek() == Some(&'"') {
                    characters.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            }
            (true, other) => field.push(other),
            (false, '"') => quoted = true,
            (false, ',') => row.push(std::mem::take(&mut field)),
            (false, '\n') => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            (false, '\r') => {}
            (false, other) => field.push(other),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// Reads every record index out of a JSON export of one range.
async fn indexes_in(repository: Arc<dyn AuditRepository>, range: ExportRange) -> Vec<i64> {
    let mut stream = Box::pin(export_stream(repository, range, ExportFormat::Json, 8));
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        bytes.extend_from_slice(&chunk.expect("chunk"));
    }
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("valid JSON");
    parsed
        .as_array()
        .expect("array")
        .iter()
        .map(|event| {
            event["metadata"]["index"]
                .as_str()
                .expect("index")
                .parse()
                .expect("numeric")
        })
        .collect()
}

/// Two adjacent exports must tile: every record exactly once across the pair,
/// none duplicated at the boundary and none lost. This is the reason the range
/// is half-open, and it is the property an auditor exporting month by month
/// actually depends on.
#[tokio::test]
async fn adjacent_ranges_tile_without_overlap_or_gap() {
    let (_directory, repository, base) = store_with(60).await;
    let boundary = base + Duration::seconds(25);

    let first = indexes_in(
        Arc::clone(&repository),
        ExportRange::new(base, boundary).expect("range"),
    )
    .await;
    let second = indexes_in(
        Arc::clone(&repository),
        ExportRange::new(boundary, base + Duration::seconds(60)).expect("range"),
    )
    .await;

    assert_eq!(first, (0..25).collect::<Vec<i64>>());
    assert_eq!(second, (25..60).collect::<Vec<i64>>());

    let mut combined = first;
    combined.extend(second);
    assert_eq!(
        combined,
        (0..60).collect::<Vec<i64>>(),
        "the two exports together cover every record exactly once"
    );
}

/// The record sitting exactly on the upper bound belongs to the next range, not
/// this one. The underlying query treats its bound as inclusive, so this is the
/// export doing the work.
#[tokio::test]
async fn a_record_on_the_upper_bound_is_excluded() {
    let (_directory, repository, base) = store_with(10).await;
    let indexes = indexes_in(
        Arc::clone(&repository),
        ExportRange::new(base, base + Duration::seconds(5)).expect("range"),
    )
    .await;
    assert_eq!(indexes, vec![0, 1, 2, 3, 4], "index 5 sits on the bound");
}
