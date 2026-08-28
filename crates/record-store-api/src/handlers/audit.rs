use axum::{
    Json,
    extract::{Extension, Query, State},
};
use record_store_audit::{AuditEvent, AuditQuery, AuditResult};
use serde::{Deserialize, Serialize};
use tracing::error;

use crate::error::ApiError;
use crate::*;

#[derive(Debug, Deserialize)]
pub(crate) struct AuditQueryParameters {
    since: Option<chrono::DateTime<chrono::Utc>>,
    until: Option<chrono::DateTime<chrono::Utc>>,
    principal: Option<String>,
    operation: Option<String>,
    resource: Option<String>,
    result: Option<AuditResult>,
    source_ip: Option<String>,
    request_id: Option<String>,
    after_time: Option<chrono::DateTime<chrono::Utc>>,
    after_id: Option<record_store_core::AuditEventId>,
    #[serde(default = "default_audit_limit")]
    limit: usize,
}

pub(crate) const fn default_audit_limit() -> usize {
    100
}

#[derive(Serialize)]
pub(crate) struct AuditEventsResponse {
    events: Vec<AuditEvent>,
    next_time: Option<chrono::DateTime<chrono::Utc>>,
    next_id: Option<record_store_core::AuditEventId>,
}

pub(crate) async fn list_audit_events(
    State(state): State<AppState>,
    Query(query): Query<AuditQueryParameters>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<AuditEventsResponse>, ApiError> {
    let after = match (query.after_time, query.after_id) {
        (Some(time), Some(id)) => Some((time, id)),
        (None, None) => None,
        _ => {
            return Err(ApiError::bad_request(
                request_id,
                "INVALID_AUDIT_CURSOR",
                "Both audit cursor fields are required",
            ));
        }
    };
    let page = state
        .audit
        .query(AuditQuery {
            since: query.since,
            until: query.until,
            principal: query.principal,
            operation: query.operation,
            resource_prefix: query.resource,
            result: query.result,
            source_ip: query.source_ip,
            request_id: query.request_id,
            after,
            limit: query.limit,
        })
        .await
        .map_err(|error| {
            error!(%error, request_id = %request_id, "audit query failed");
            ApiError::bad_request(request_id, "INVALID_AUDIT_QUERY", "Invalid audit query")
        })?;
    let (next_time, next_id) = page
        .next
        .map_or((None, None), |(time, id)| (Some(time), Some(id)));
    Ok(Json(AuditEventsResponse {
        events: page.events,
        next_time,
        next_id,
    }))
}
