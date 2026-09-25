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
    /// Whether the server stopped scanning before reaching the end of the
    /// requested range.
    ///
    /// A filtered query walks the range record by record and gives up after a
    /// bounded number of them. When that happens the page can be short, or
    /// empty, while matches remain further on — so the caller is told, and
    /// given a cursor, rather than left to read an empty page as an answer.
    scan_truncated: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChainQueryParameters {
    #[serde(default)]
    from: u64,
    #[serde(default = "default_chain_limit")]
    limit: usize,
}

const fn default_chain_limit() -> usize {
    1_000
}

#[derive(Serialize)]
pub(crate) struct ChainProblemEntry {
    sequence: u64,
    verdict: &'static str,
}

#[derive(Serialize)]
pub(crate) struct ChainVerificationResponse {
    /// Whether every record examined in this span verified.
    ///
    /// Scoped to the span: a walk that did not start at sequence zero took the
    /// preceding record's stored hash on trust, and a walk that stopped on its
    /// limit says so through `next_from`.
    intact: bool,
    from: u64,
    checked: u64,
    verified: u64,
    /// Records predating the chain, which carry no links and are not evidence.
    unchained: u64,
    next_from: Option<u64>,
    head_sequence: Option<u64>,
    problems: Vec<ChainProblemEntry>,
}

/// Recomputes the audit hash chain over a span of the log.
///
/// What this establishes is bounded and worth stating: it detects a record
/// that was edited, removed, or reordered by anyone who could not also rewrite
/// every later link. It does not detect an operator who rewrote the whole log
/// and every hash in it. Only an external anchor over a checkpoint reaches
/// that far, and this deployment does not yet produce one.
pub(crate) async fn verify_audit_chain(
    State(state): State<AppState>,
    Query(query): Query<ChainQueryParameters>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<ChainVerificationResponse>, ApiError> {
    let verification = state
        .audit
        .verify_chain(query.from, query.limit)
        .await
        .map_err(|error| {
            error!(%error, request_id = %request_id, "audit chain verification failed");
            ApiError::bad_request(
                request_id,
                "INVALID_AUDIT_CHAIN_REQUEST",
                "Invalid audit chain verification request",
            )
        })?;
    Ok(Json(ChainVerificationResponse {
        intact: verification.is_intact(),
        from: verification.from_sequence,
        checked: verification.checked,
        verified: verification.intact,
        unchained: verification.unchained,
        next_from: verification.next_sequence,
        head_sequence: verification.head_sequence,
        problems: verification
            .problems
            .into_iter()
            .map(|problem| ChainProblemEntry {
                sequence: problem.sequence,
                verdict: problem.verdict,
            })
            .collect(),
    }))
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
        scan_truncated: page.scan_truncated,
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use async_trait::async_trait;
    use axum::http::StatusCode;
    use record_store_audit::{
        AuditError, AuditEvent, AuditPage, AuditQuery, AuditRepository, ChainVerification,
        RedbAuditRepository, intent::INTENT_EVENT_ID,
    };
    use serde_json::json;

    use crate::test_support::{
        AUDITOR_TOKEN, admin, api, api_with_audit, call, expect_status, signed,
    };

    /// An audit trail that can be made to fail on demand.
    struct FaultyAudit {
        inner: RedbAuditRepository,
        failing: AtomicBool,
    }

    #[async_trait]
    impl AuditRepository for FaultyAudit {
        async fn append(&self, event: &AuditEvent) -> Result<(), AuditError> {
            if self.failing.load(Ordering::Relaxed) {
                return Err(AuditError::Database {
                    operation: "append",
                    reason: "injected failure".into(),
                });
            }
            self.inner.append(event).await
        }

        async fn query(&self, query: AuditQuery) -> Result<AuditPage, AuditError> {
            self.inner.query(query).await
        }

        async fn verify_chain(
            &self,
            from_sequence: u64,
            limit: usize,
        ) -> Result<ChainVerification, AuditError> {
            self.inner.verify_chain(from_sequence, limit).await
        }

        async fn check_ready(&self) -> Result<(), AuditError> {
            self.inner.check_ready().await
        }
    }

    #[tokio::test]
    async fn the_audit_trail_is_readable_and_starts_empty() {
        let (_directory, api) = api().await;
        let events = expect_status(
            &api,
            admin("GET", "/api/v1/audit/events", None),
            StatusCode::OK,
        )
        .await;
        assert!(events["events"].as_array().expect("events").is_empty());
    }

    /// Reading the audit trail is exactly what an auditor exists to do, so the
    /// auditor role must reach it.
    #[tokio::test]
    async fn an_auditor_can_read_the_audit_trail() {
        let (_directory, api) = api().await;
        let response = call(
            &api,
            signed("GET", "/api/v1/audit/events", AUDITOR_TOKEN, None),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn audit_filters_are_accepted_and_bounded() {
        let (_directory, api) = api().await;
        expect_status(
            &api,
            admin(
                "GET",
                "/api/v1/audit/events?limit=5&principal=root&operation=DeleteBucket",
                None,
            ),
            StatusCode::OK,
        )
        .await;
    }

    /// Administrative changes have to leave a trail; an audit log that stays
    /// empty while accounts are created is worse than no log at all.
    #[tokio::test]
    async fn administrative_changes_are_recorded() {
        let (_directory, api) = api().await;
        expect_status(
            &api,
            admin(
                "POST",
                "/api/v1/service-accounts",
                Some(json!({"name": "audited"})),
            ),
            StatusCode::CREATED,
        )
        .await;

        let events = expect_status(
            &api,
            admin("GET", "/api/v1/audit/events", None),
            StatusCode::OK,
        )
        .await;
        assert!(
            !events["events"].as_array().expect("events").is_empty(),
            "creating an account must be audited: {events}"
        );
        assert_eq!(
            events["scan_truncated"], false,
            "a short range is walked to its end: {events}"
        );
    }

    /// The trail is only evidence if it can be checked. The endpoint has to
    /// report the chain over records this deployment actually wrote, and it has
    /// to be reachable by the role whose job it is to ask.
    #[tokio::test]
    async fn the_audit_chain_can_be_verified_by_an_administrator_and_an_auditor() {
        let (_directory, api) = api().await;
        expect_status(
            &api,
            admin(
                "POST",
                "/api/v1/service-accounts",
                Some(json!({"name": "chained"})),
            ),
            StatusCode::CREATED,
        )
        .await;

        let report = expect_status(
            &api,
            admin("GET", "/api/v1/audit/chain", None),
            StatusCode::OK,
        )
        .await;
        assert_eq!(report["intact"], true, "{report}");
        assert!(
            report["checked"].as_u64().expect("checked") > 0,
            "the chain must cover the records this test produced: {report}"
        );
        assert_eq!(report["unchained"], 0, "{report}");
        assert!(
            report["problems"].as_array().expect("problems").is_empty(),
            "{report}"
        );

        let auditor = call(
            &api,
            signed("GET", "/api/v1/audit/chain", AUDITOR_TOKEN, None),
        )
        .await;
        assert_eq!(auditor.status(), StatusCode::OK);
    }

    /// An administrative change announces itself before it happens. The pair
    /// is what makes a crash between the change and its outcome visible rather
    /// than silent.
    #[tokio::test]
    async fn an_administrative_mutation_writes_an_intent_before_its_outcome() {
        let (_directory, api) = api().await;
        expect_status(
            &api,
            admin(
                "POST",
                "/api/v1/service-accounts",
                Some(json!({"name": "paired"})),
            ),
            StatusCode::CREATED,
        )
        .await;

        let listed = expect_status(
            &api,
            admin("GET", "/api/v1/audit/events?limit=100", None),
            StatusCode::OK,
        )
        .await;
        let events = listed["events"].as_array().expect("events");
        let intent = events
            .iter()
            .find(|event| event["result"] == "attempted")
            .unwrap_or_else(|| panic!("no intent record was written: {listed}"));
        let outcome = events
            .iter()
            .find(|event| event["metadata"][INTENT_EVENT_ID] == intent["event_id"])
            .unwrap_or_else(|| panic!("no outcome names the intent: {listed}"));

        assert_eq!(intent["operation"], "POST /api/v1/service-accounts");
        assert_eq!(outcome["result"], "success");
        assert_eq!(outcome["request_id"], intent["request_id"]);
        assert_eq!(outcome["principal"], intent["principal"]);
    }

    /// A read changes nothing, so it keeps its single record.
    #[tokio::test]
    async fn a_management_read_writes_no_intent() {
        let (_directory, api) = api().await;
        expect_status(&api, admin("GET", "/api/v1/buckets", None), StatusCode::OK).await;
        let listed = expect_status(
            &api,
            admin("GET", "/api/v1/audit/events?limit=100", None),
            StatusCode::OK,
        )
        .await;
        assert!(
            listed["events"]
                .as_array()
                .expect("events")
                .iter()
                .all(|event| event["result"] != "attempted"),
            "{listed}"
        );
    }

    /// When the trail cannot be written, the change is refused rather than made
    /// unaccountably. A service account that could not be announced must not
    /// exist afterwards.
    #[tokio::test]
    async fn an_administrative_mutation_is_refused_when_its_intent_cannot_be_written() {
        let staging = tempfile::tempdir().expect("temporary directory");
        let audit = Arc::new(FaultyAudit {
            inner: RedbAuditRepository::open(staging.path().join("audit.redb"))
                .await
                .expect("audit"),
            failing: AtomicBool::new(false),
        });
        let (_directory, api, _handle) =
            api_with_audit(Some(audit.clone() as Arc<dyn AuditRepository>)).await;

        audit.failing.store(true, Ordering::Relaxed);
        let refused = call(
            &api,
            admin(
                "POST",
                "/api/v1/service-accounts",
                Some(json!({"name": "unrecordable"})),
            ),
        )
        .await;
        assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);

        audit.failing.store(false, Ordering::Relaxed);
        let accounts = expect_status(
            &api,
            admin("GET", "/api/v1/service-accounts", None),
            StatusCode::OK,
        )
        .await;
        assert!(
            !accounts.to_string().contains("unrecordable"),
            "the refused account must not exist"
        );
    }
}
