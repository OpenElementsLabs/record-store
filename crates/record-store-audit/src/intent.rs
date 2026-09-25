//! The two records a mutating request leaves behind.
//!
//! An audit record written after a mutation commits can be lost: the process
//! can die in the window between the commit and the append, and the append
//! itself can fail against a full or broken disk. Either way the mutation
//! happened and nothing says so, which is the one failure an audit trail is not
//! allowed to have.
//!
//! So a mutating request writes twice. Before it is attempted it writes an
//! *intent* — [`AuditResult::Attempted`] — naming who asked, what for, and
//! when. That record is durable before a single byte of the mutation is. Once
//! the outcome is known it writes a second record carrying the real result and
//! pointing back at the intent.
//!
//! The pair is what makes the failure legible instead of invisible:
//!
//! | What is in the log | What it means |
//! | --- | --- |
//! | intent and outcome | the ordinary case |
//! | intent alone | the server cannot account for this operation — it may have committed |
//! | outcome alone | impossible, and evidence the log was edited |
//!
//! An intent with no outcome is deliberately *not* resolved by the server after
//! the fact. It has no way to know what happened either, and writing a guess
//! would turn an honest gap into a false record.

use std::collections::BTreeMap;

use chrono::Utc;
use record_store_core::AuditEventId;

use crate::{AuditEvent, AuditResult};

/// Metadata key on an outcome record naming the intent it completes.
///
/// The request identifier already pairs them for a human reading the log. This
/// is for everything else: it survives a request identifier being reused, and
/// it lets a reader follow the pair without knowing how request identifiers are
/// allocated.
pub const INTENT_EVENT_ID: &str = "intent_event_id";

/// Whether an operation changes durable state and therefore needs an intent.
///
/// Decided from the HTTP method alone, deliberately. A per-route list would
/// drift the moment a route is added, and the direction it drifts in is a
/// mutation that quietly stops being announced.
#[must_use]
pub fn method_mutates(method: &str) -> bool {
    !matches!(method, "GET" | "HEAD" | "OPTIONS" | "TRACE")
}

/// Builds the record written before a mutation is attempted.
#[must_use]
pub fn intent_event(
    request_id: Option<String>,
    principal: String,
    credential_id: Option<uuid::Uuid>,
    source_ip: Option<String>,
    operation: String,
    resource: String,
) -> AuditEvent {
    AuditEvent {
        event_id: AuditEventId::new(),
        timestamp: Utc::now(),
        request_id,
        principal,
        credential_id,
        source_ip,
        operation,
        resource,
        result: AuditResult::Attempted,
        metadata: BTreeMap::new(),
    }
}

/// Builds the record written once the outcome of a mutation is known.
///
/// Every identifying field is carried over from the intent so the two records
/// describe the same operation even if the request's own state has moved on.
#[must_use]
pub fn outcome_event(intent: &AuditEvent, result: AuditResult) -> AuditEvent {
    let mut metadata = intent.metadata.clone();
    metadata.insert(INTENT_EVENT_ID.to_owned(), intent.event_id.to_string());
    AuditEvent {
        event_id: AuditEventId::new(),
        timestamp: Utc::now(),
        result,
        metadata,
        ..intent.clone()
    }
}

/// Maps an HTTP status onto the audit result categories.
#[must_use]
pub const fn result_for_status(status: u16) -> AuditResult {
    match status {
        401 | 403 => AuditResult::Denied,
        200..=299 => AuditResult::Success,
        _ => AuditResult::Failure,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_state_changing_methods_need_an_intent() {
        for method in ["POST", "PUT", "PATCH", "DELETE"] {
            assert!(method_mutates(method), "{method}");
        }
        for method in ["GET", "HEAD", "OPTIONS", "TRACE"] {
            assert!(!method_mutates(method), "{method}");
        }
    }

    /// The outcome has to be recognisably the same operation as its intent, or
    /// pairing them means guessing.
    #[test]
    fn an_outcome_carries_its_intent_forward_and_names_it() {
        let intent = intent_event(
            Some("req-1".to_owned()),
            "service_account:abc".to_owned(),
            None,
            Some("203.0.113.7".to_owned()),
            "s3:DELETE".to_owned(),
            "/records/statement.pdf".to_owned(),
        );
        assert_eq!(intent.result, AuditResult::Attempted);

        let outcome = outcome_event(&intent, AuditResult::Success);
        assert_eq!(outcome.request_id, intent.request_id);
        assert_eq!(outcome.principal, intent.principal);
        assert_eq!(outcome.operation, intent.operation);
        assert_eq!(outcome.resource, intent.resource);
        assert_eq!(outcome.source_ip, intent.source_ip);
        assert_eq!(outcome.result, AuditResult::Success);
        assert_ne!(outcome.event_id, intent.event_id);
        assert_eq!(
            outcome.metadata.get(INTENT_EVENT_ID),
            Some(&intent.event_id.to_string())
        );
    }

    #[test]
    fn statuses_map_to_the_documented_result_categories() {
        assert_eq!(result_for_status(200), AuditResult::Success);
        assert_eq!(result_for_status(204), AuditResult::Success);
        assert_eq!(result_for_status(401), AuditResult::Denied);
        assert_eq!(result_for_status(403), AuditResult::Denied);
        assert_eq!(result_for_status(404), AuditResult::Failure);
        assert_eq!(result_for_status(500), AuditResult::Failure);
    }
}
