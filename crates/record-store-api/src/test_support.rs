//! Shared fixtures for management API tests.
//!
//! The router is built over real backends in a throwaway directory so the tests
//! exercise the same code paths a deployment does, including authorization and
//! durable side effects.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use http_body_util::BodyExt;
use record_store_audit::{AuditRepository, RedbAuditRepository};
use record_store_auth::CredentialManager;
use record_store_core::OrganizationId;
use record_store_events::{EventRepository, RedbEventRepository};
use record_store_metadata::{MetadataRepository, RedbMetadataRepository};
use record_store_service::{ServiceLimits, Services};
use record_store_storage::{LocalFilesystemStore, ObjectStore};
use tempfile::TempDir;
use tower::ServiceExt;

use crate::{AppState, ManagementAuth, router};

/// The system-administrator token every authenticated fixture request presents.
pub(crate) const SYSTEM_TOKEN: &str = "system-token-at-least-thirty-two-bytes-long";
/// A storage-administrator token, for tests that check role separation.
pub(crate) const STORAGE_TOKEN: &str = "storage-token-at-least-thirty-two-bytes-long";
/// An auditor token, which may only read.
pub(crate) const AUDITOR_TOKEN: &str = "auditor-token-at-least-thirty-two-bytes-long";

/// Builds the management router over real catalog, storage, and audit backends.
pub(crate) async fn api() -> (TempDir, Router) {
    let directory = tempfile::tempdir().expect("temporary directory");
    let metadata: Arc<dyn MetadataRepository> = Arc::new(
        RedbMetadataRepository::open(directory.path().join("metadata.redb"))
            .await
            .expect("metadata repository"),
    );
    let storage: Arc<dyn ObjectStore> = Arc::new(
        LocalFilesystemStore::open(
            directory.path(),
            directory.path().join("tmp"),
            Arc::clone(&metadata),
        )
        .await
        .expect("filesystem store"),
    );
    let audit: Arc<dyn AuditRepository> = Arc::new(
        RedbAuditRepository::open(directory.path().join("audit.redb"))
            .await
            .expect("audit repository"),
    );
    let events: Arc<dyn EventRepository> = Arc::new(
        RedbEventRepository::open(
            directory.path().join("events.redb"),
            Some(b"event-master-key-at-least-32-bytes-long"),
            // Loopback delivery is permitted so tests can register a target
            // that actually resolves; the public-network policy is exercised
            // separately by `api_with_public_webhooks_only`.
            record_store_events::WebhookConfig {
                allow_http: true,
                allow_private_networks: true,
                ..record_store_events::WebhookConfig::default()
            },
        )
        .await
        .expect("event repository"),
    );
    let credentials = Arc::new(
        CredentialManager::open(
            directory.path().join("credentials.redb"),
            "root-access-key",
            "root-secret-at-least-sixteen",
            Some(b"credential-master-key-at-least-32-bytes"),
        )
        .await
        .expect("credential manager"),
    );
    let owner = OrganizationId::new();
    let services = Services::new(
        Arc::clone(&storage),
        Arc::clone(&metadata),
        owner,
        ServiceLimits {
            maximum_concurrent_operations: 8,
            maximum_custom_metadata_entries: 8,
            maximum_custom_metadata_bytes: 1_024,
        },
    );
    let state = AppState::new(
        storage,
        metadata,
        services,
        credentials,
        audit,
        owner,
        "0.0.0-test",
    )
    .with_events(events)
    .with_management_auth(ManagementAuth::bearer_tokens(
        SYSTEM_TOKEN.as_bytes(),
        Some(STORAGE_TOKEN.as_bytes()),
        Some(AUDITOR_TOKEN.as_bytes()),
    ));
    (directory, router(state))
}

/// Sends a request as the system administrator.
pub(crate) async fn call(router: &Router, request: Request<Body>) -> Response<Body> {
    router
        .clone()
        .oneshot(request)
        .await
        .expect("router responds")
}

/// Builds an authenticated request carrying an optional JSON body.
pub(crate) fn signed(
    method: &str,
    uri: &str,
    token: &str,
    body: Option<serde_json::Value>,
) -> Request<Body> {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"));
    match body {
        Some(value) => builder
            .header("content-type", "application/json")
            .body(Body::from(value.to_string()))
            .expect("request"),
        None => builder.body(Body::empty()).expect("request"),
    }
}

/// Builds a request as the system administrator.
pub(crate) fn admin(method: &str, uri: &str, body: Option<serde_json::Value>) -> Request<Body> {
    signed(method, uri, SYSTEM_TOKEN, body)
}

/// Reads a response body as JSON.
pub(crate) async fn json_body(response: Response<Body>) -> serde_json::Value {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    if bytes.is_empty() {
        return serde_json::Value::Null;
    }
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| serde_json::Value::String(String::from_utf8_lossy(&bytes).into_owned()))
}

/// Sends an authenticated request and asserts the resulting status.
pub(crate) async fn expect_status(
    router: &Router,
    request: Request<Body>,
    expected: StatusCode,
) -> serde_json::Value {
    let response = call(router, request).await;
    let status = response.status();
    let body = json_body(response).await;
    assert_eq!(status, expected, "unexpected status; body was {body}");
    body
}
