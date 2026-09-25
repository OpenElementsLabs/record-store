//! Auditor-facing exports: a streamed copy of an audit range, and a report of
//! what Object Lock currently holds.
//!
//! Both are readable with the auditor role, because they are the questions an
//! auditor exists to ask, and both are read-only.

use axum::{
    Json,
    extract::{Extension, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use record_store_audit::export::{
    CheckpointCoverage, EXPORT_PAGE_SIZE, ExportFormat, ExportManifest, ExportRange,
    MANIFEST_FORMAT, MANIFEST_VERSION, export_stream,
};
use record_store_audit::{AuditEvent, AuditResult};
use record_store_core::AuditEventId;
use record_store_service::RetentionReport;
use serde::Deserialize;
use std::collections::BTreeMap;

use crate::auth::ManagementPrincipal;
use crate::error::{ApiError, service_to_api_error};
use crate::*;

/// The range and format an export names.
#[derive(Deserialize)]
pub(crate) struct ExportParameters {
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    #[serde(default = "default_format")]
    format: String,
}

fn default_format() -> String {
    "json".to_owned()
}

impl ExportParameters {
    /// Validates the range and format an auditor asked for.
    fn resolve(&self, request_id: &RequestId) -> Result<(ExportRange, ExportFormat), ApiError> {
        let range = ExportRange::new(self.from, self.to).map_err(|_| {
            ApiError::bad_request(
                request_id.clone(),
                "INVALID_EXPORT_RANGE",
                "An export range must end after it begins",
            )
        })?;
        let format = ExportFormat::parse(&self.format).map_err(|_| {
            ApiError::bad_request(
                request_id.clone(),
                "INVALID_EXPORT_FORMAT",
                "An export format must be json or csv",
            )
        })?;
        Ok((range, format))
    }
}

/// Describes an export, and records that one was authorized.
///
/// The audit record is written here rather than when the bytes finish, so it
/// says an export was *requested and permitted*. It does not claim the export
/// was delivered: a transfer that fails midway still represents a range being
/// handed out, and that is the fact worth keeping.
pub(crate) async fn export_manifest(
    State(state): State<AppState>,
    Query(parameters): Query<ExportParameters>,
    Extension(principal): Extension<ManagementPrincipal>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<ExportManifest>, ApiError> {
    let (range, format) = parameters.resolve(&request_id)?;
    let export_id = uuid::Uuid::new_v4().to_string();
    let exported_by = principal.audit_name().to_owned();
    let exported_at = Utc::now();

    let mut metadata = BTreeMap::new();
    metadata.insert("export_id".to_owned(), export_id.clone());
    metadata.insert("from".to_owned(), range.from.to_rfc3339());
    metadata.insert("to".to_owned(), range.to.to_rfc3339());
    metadata.insert("format".to_owned(), format.as_str().to_owned());
    let event = AuditEvent {
        event_id: AuditEventId::new(),
        timestamp: exported_at,
        request_id: Some(request_id.to_string()),
        principal: exported_by.clone(),
        credential_id: None,
        source_ip: None,
        operation: "audit.export".to_owned(),
        resource: "audit".to_owned(),
        result: AuditResult::Success,
        metadata,
    };
    if let Err(error) = state.audit.append(&event).await {
        // An export that leaves no trace is exactly what this record exists to
        // prevent, so the request fails rather than proceeding untracked.
        tracing::error!(%error, "durable audit export record failed");
        return Err(ApiError::internal(request_id));
    }

    Ok(Json(ExportManifest {
        format: MANIFEST_FORMAT.to_owned(),
        manifest_version: MANIFEST_VERSION,
        export_id,
        exported_by,
        exported_at,
        range,
        record_format: format.as_str().to_owned(),
        record_file: format.file_name().to_owned(),
        // No audit chain exists yet, so no checkpoint covers the range. Said
        // explicitly, with what it means, rather than left out.
        checkpoints: CheckpointCoverage::not_checkpointed(),
    }))
}

/// Streams the records in a range.
///
/// One bounded page is held at a time; the range never is. An auditor asking
/// for a year of a busy deployment gets a long response, not an outage.
pub(crate) async fn export_records(
    State(state): State<AppState>,
    Query(parameters): Query<ExportParameters>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Response, ApiError> {
    let (range, format) = parameters.resolve(&request_id)?;
    let stream = export_stream(Arc::clone(&state.audit), range, format, EXPORT_PAGE_SIZE);
    let body = axum::body::Body::from_stream(stream);
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, format.content_type()),
            (
                header::CONTENT_DISPOSITION,
                // Always an attachment: an audit export is a file to keep, not
                // a page to render.
                "attachment",
            ),
        ],
        body,
    )
        .into_response())
}

/// Reports which buckets have Object Lock and what it currently holds.
pub(crate) async fn retention_report(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<RetentionReport>, ApiError> {
    state
        .services
        .locks
        .retention_report()
        .await
        .map(Json)
        .map_err(|error| service_to_api_error(error, request_id))
}
