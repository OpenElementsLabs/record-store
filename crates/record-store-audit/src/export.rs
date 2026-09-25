//! Streaming an audit range out for an auditor.
//!
//! An export exists to leave the deployment, so two properties matter more than
//! anything else here. It must never hold the range in memory — an audit log
//! grows without bound and an auditor asking for a year of it should not be
//! able to take the node down. And what comes out must be something an auditor
//! can open with ordinary tools, which is why the JSON form is a real JSON
//! document rather than a stream of concatenated objects.
//!
//! What an export is *not* is proof. It is a copy of what this server says the
//! log contains. The `SHA256SUMS` written alongside it establishes that the copy
//! reached you unaltered; it establishes nothing about whether the log was
//! edited before the copy was taken. Until a checkpoint covers the range, that
//! distinction is the whole story, and `docs/administration/audit-export.md`
//! states it rather than leaving it to be inferred.

use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_core::Stream;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{AuditError, AuditEvent, AuditQuery, AuditRepository, AuditResult};

/// Records fetched per page while streaming.
///
/// The repository caps a single query, so this only decides how often the
/// export goes back for more, not how much it may hold.
pub const EXPORT_PAGE_SIZE: usize = 500;

/// Wire format an export is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    /// A single JSON array, streamed incrementally.
    Json,
    /// RFC 4180 CSV with a pinned column order.
    Csv,
}

impl ExportFormat {
    /// Parses the format named on a command line or query string.
    pub fn parse(value: &str) -> Result<Self, AuditError> {
        match value {
            "json" => Ok(Self::Json),
            "csv" => Ok(Self::Csv),
            _ => Err(AuditError::InvalidExportFormat),
        }
    }

    /// Returns the name used in file names and manifests.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Csv => "csv",
        }
    }

    /// Returns the file name an export of this format is written to.
    #[must_use]
    pub const fn file_name(self) -> &'static str {
        match self {
            Self::Json => "audit.json",
            Self::Csv => "audit.csv",
        }
    }

    /// Returns the media type the management API serves it as.
    #[must_use]
    pub const fn content_type(self) -> &'static str {
        match self {
            Self::Json => "application/json",
            Self::Csv => "text/csv",
        }
    }
}

/// The time range an export covers: `[from, to)`.
///
/// Half-open on purpose. An auditor exporting January and then February must
/// receive every record exactly once across the two, and a closed upper bound
/// would hand them any record landing exactly on the boundary twice. The
/// underlying audit query treats its upper bound as inclusive, so the export
/// filters the boundary itself rather than relying on timestamp precision to
/// do it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportRange {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

impl ExportRange {
    /// Builds a range, refusing one that runs backwards.
    pub fn new(from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Self, AuditError> {
        if to <= from {
            return Err(AuditError::InvalidExportRange);
        }
        Ok(Self { from, to })
    }
}

/// Whether the exported range is covered by a checkpoint.
///
/// A tagged union rather than an optional field, for the same reason the proof
/// bundle uses one: an omitted section reads as "nothing to report", where this
/// reads as "this was not established".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CheckpointCoverage {
    /// No checkpoint covers the range, and why.
    Unavailable {
        /// Machine-readable reason.
        reason: String,
        /// Sentence an auditor can act on.
        detail: String,
    },
    /// Checkpoint roots covering the range.
    Present {
        /// Roots, in checkpoint order.
        checkpoints: Vec<ExportedCheckpoint>,
    },
}

impl CheckpointCoverage {
    /// Returns the coverage a deployment without checkpoints reports.
    ///
    /// The log is hash-chained, which is a real property and a narrower one
    /// than an auditor might assume from an export arriving with digests
    /// attached. Naming the gap precisely is the point of the field.
    #[must_use]
    pub fn not_checkpointed() -> Self {
        Self::Unavailable {
            reason: "not_yet_checkpointed".to_owned(),
            detail: "this deployment maintains a hash-chained audit log but does not yet \
                     produce checkpoints, so no Merkle root covers this range. The \
                     SHA256SUMS file establishes that this copy reached you unaltered. The \
                     chain establishes that no record was edited or removed by anyone who \
                     could not also rewrite every later link — verify it with \
                     GET /api/v1/audit/chain. Neither establishes anything against an \
                     operator who rewrote the whole log and every hash in it; only an \
                     external anchor over a checkpoint reaches that far."
                .to_owned(),
        }
    }
}

/// One checkpoint root covering part of an exported range.
///
/// Carries the leaf count for the same reason the checkpoint itself does: a
/// root without the number of leaves under it cannot be checked for having
/// quietly lost one. See [`crate::checkpoint::Checkpoint`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportedCheckpoint {
    pub sequence: u64,
    pub from_sequence: u64,
    pub to_sequence: u64,
    pub leaf_count: u64,
    pub root: String,
}

/// What an export contains, written alongside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportManifest {
    /// Identifies the document.
    pub format: String,
    /// Version of the manifest shape.
    pub manifest_version: u16,
    /// A stable identifier for this export, also used in its audit record.
    pub export_id: String,
    /// Who asked for it.
    pub exported_by: String,
    /// When the export was authorized.
    pub exported_at: DateTime<Utc>,
    /// The range covered.
    pub range: ExportRange,
    /// The wire format.
    pub record_format: String,
    /// The file the records were written to.
    pub record_file: String,
    /// Whether a checkpoint covers the range.
    pub checkpoints: CheckpointCoverage,
}

/// Identifies an export manifest to anything that reads one.
pub const MANIFEST_FORMAT: &str = "record-store.audit-export-manifest";
/// Version of the manifest shape this release writes.
pub const MANIFEST_VERSION: u16 = 1;

/// Returns the CSV header, pinned so a consumer can rely on the column order.
#[must_use]
pub const fn csv_header() -> &'static str {
    "timestamp,event_id,principal,credential_id,source_ip,request_id,operation,resource,result,metadata\n"
}

/// Quotes one CSV field per RFC 4180.
fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

/// Renders the stable wire name of an audit result.
const fn result_name(result: AuditResult) -> &'static str {
    match result {
        AuditResult::Success => "success",
        AuditResult::Denied => "denied",
        AuditResult::Failure => "failure",
        AuditResult::Attempted => "attempted",
    }
}

/// Encodes one event as a CSV row.
///
/// The metadata map becomes a single JSON column. Flattening it into columns
/// would make the column set depend on the data, so two exports of the same
/// deployment could disagree about their own shape.
fn csv_row(event: &AuditEvent) -> Result<String, AuditError> {
    let metadata = serde_json::to_string(&event.metadata)?;
    let fields = [
        event.timestamp.to_rfc3339(),
        event.event_id.to_string(),
        event.principal.clone(),
        event
            .credential_id
            .map(|id| id.to_string())
            .unwrap_or_default(),
        event.source_ip.clone().unwrap_or_default(),
        event.request_id.clone().unwrap_or_default(),
        event.operation.clone(),
        event.resource.clone(),
        result_name(event.result).to_owned(),
        metadata,
    ];
    let mut row = String::with_capacity(256);
    for (index, field) in fields.iter().enumerate() {
        if index > 0 {
            row.push(',');
        }
        row.push_str(&csv_field(field));
    }
    row.push('\n');
    Ok(row)
}

/// Streaming state, kept small because it is all the export is allowed to hold.
struct ExportState {
    repository: Arc<dyn AuditRepository>,
    range: ExportRange,
    format: ExportFormat,
    page_size: usize,
    cursor: Option<(DateTime<Utc>, crate::AuditEventId)>,
    started: bool,
    finished: bool,
    /// Records written so far, which decides where a JSON separator goes.
    emitted: u64,
}

/// Streams an audit range as encoded bytes.
///
/// One bounded page is held at a time, never the range. The JSON form opens and
/// closes its array around the pages so the result is a single valid document.
pub fn export_stream(
    repository: Arc<dyn AuditRepository>,
    range: ExportRange,
    format: ExportFormat,
    page_size: usize,
) -> impl Stream<Item = Result<Bytes, AuditError>> + Send {
    let state = ExportState {
        repository,
        range,
        format,
        page_size: page_size.clamp(1, 1_000),
        cursor: None,
        started: false,
        finished: false,
        emitted: 0,
    };
    futures_util::stream::try_unfold(state, |mut state| async move {
        if state.finished {
            return Ok(None);
        }
        let mut chunk = Vec::with_capacity(8 * 1024);
        if !state.started {
            state.started = true;
            match state.format {
                ExportFormat::Json => chunk.push(b'['),
                ExportFormat::Csv => chunk.extend_from_slice(csv_header().as_bytes()),
            }
        }
        let page = state
            .repository
            .query(AuditQuery {
                since: Some(state.range.from),
                until: Some(state.range.to),
                after: state.cursor,
                limit: state.page_size,
                ..AuditQuery::default()
            })
            .await?;
        for event in &page.events {
            // The store's upper bound is inclusive, so the boundary record is
            // dropped here to keep the range half-open.
            if event.timestamp >= state.range.to {
                continue;
            }
            match state.format {
                ExportFormat::Json => {
                    if state.emitted > 0 {
                        chunk.push(b',');
                    }
                    serde_json::to_writer(&mut chunk, event)?;
                }
                ExportFormat::Csv => chunk.extend_from_slice(csv_row(event)?.as_bytes()),
            }
            state.emitted += 1;
        }
        match page.next {
            Some(next) => state.cursor = Some(next),
            None => {
                state.finished = true;
                if state.format == ExportFormat::Json {
                    chunk.push(b']');
                }
            }
        }
        Ok(Some((Bytes::from(chunk), state)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_fields_are_quoted_per_rfc_4180() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("with,comma"), "\"with,comma\"");
        assert_eq!(csv_field("with\"quote"), "\"with\"\"quote\"");
        assert_eq!(csv_field("with\nnewline"), "\"with\nnewline\"");
        assert_eq!(csv_field(""), "");
    }

    /// The column order is a contract with whoever consumes an export.
    #[test]
    fn the_csv_header_is_pinned() {
        assert_eq!(
            csv_header(),
            "timestamp,event_id,principal,credential_id,source_ip,request_id,\
             operation,resource,result,metadata\n"
                .replace(",\\\n             ", ",")
                .as_str()
        );
        assert_eq!(csv_header().matches(',').count(), 9);
    }

    #[test]
    fn result_names_are_stable() {
        assert_eq!(result_name(AuditResult::Success), "success");
        assert_eq!(result_name(AuditResult::Denied), "denied");
        assert_eq!(result_name(AuditResult::Failure), "failure");
        assert_eq!(result_name(AuditResult::Attempted), "attempted");
    }

    #[test]
    fn a_range_that_runs_backwards_is_refused() {
        let now = Utc::now();
        assert!(ExportRange::new(now, now).is_err());
        assert!(ExportRange::new(now, now - chrono::Duration::hours(1)).is_err());
        assert!(ExportRange::new(now - chrono::Duration::hours(1), now).is_ok());
    }

    #[test]
    fn formats_parse_and_name_their_files() {
        assert_eq!(
            ExportFormat::parse("json").expect("json"),
            ExportFormat::Json
        );
        assert_eq!(ExportFormat::parse("csv").expect("csv"), ExportFormat::Csv);
        assert!(ExportFormat::parse("xml").is_err());
        assert_eq!(ExportFormat::Json.file_name(), "audit.json");
        assert_eq!(ExportFormat::Csv.file_name(), "audit.csv");
    }

    /// The reason a range is uncovered has to travel with the export, or an
    /// auditor cannot tell "no checkpoint" from "checkpoint omitted".
    #[test]
    fn uncovered_ranges_carry_an_explicit_reason() {
        let coverage = CheckpointCoverage::not_checkpointed();
        let encoded = serde_json::to_string(&coverage).expect("encode");
        assert!(encoded.contains("\"status\":\"unavailable\""), "{encoded}");
        assert!(encoded.contains("not_yet_checkpointed"), "{encoded}");
        assert!(
            encoded.contains("Neither establishes anything against an operator"),
            "the limitation travels with the document: {encoded}"
        );
        assert!(
            encoded.contains("hash-chained"),
            "and so does the property that does hold: {encoded}"
        );
    }
}
