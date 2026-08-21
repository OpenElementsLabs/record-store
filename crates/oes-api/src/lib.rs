//! Native operational and authenticated management HTTP API.

use std::{
    fmt::{self, Display, Formatter},
    future::{Future, IntoFuture},
    net::SocketAddr,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    extract::{ConnectInfo, DefaultBodyLimit, Extension, Path, Query, Request, State},
    http::{HeaderValue, StatusCode, header, header::HeaderName},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use oes_audit::{AuditEvent, AuditQuery, AuditRepository, AuditResult};
use oes_auth::{CredentialManager, Policy, PolicyStatement, ServiceAccountInfo};
use oes_core::{
    Bucket, BucketName, BucketQuota, ExpirationDays, LifecycleRule, LifecycleRuleId, ObjectKey,
    ObjectMetadata, OrganizationId, ServiceAccountId, StorageUsage, VersionId, VersioningState,
    WebhookId,
};
use oes_events::{
    CreateWebhookRequest, CreatedWebhook, EventRepository, WebhookDeliveryLog, WebhookSubscription,
};
use oes_metadata::MetadataRepository;
use oes_service::{ServiceError, ServiceListRequest, Services};
use oes_storage::{
    ObjectStore, StorageInspection, StorageRepairRequest, StorageRepairResult, StorageStatus,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use thiserror::Error;
use tokio::{net::TcpListener, time::timeout};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, error, info, info_span};
use uuid::Uuid;

static REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

/// Explicit dependencies shared by native HTTP handlers.
#[derive(Clone)]
pub struct AppState {
    storage: Arc<dyn ObjectStore>,
    metadata: Arc<dyn MetadataRepository>,
    services: Services,
    credentials: Arc<CredentialManager>,
    audit: Arc<dyn AuditRepository>,
    owner: OrganizationId,
    version: &'static str,
    management_auth: ManagementAuth,
    events: Option<Arc<dyn EventRepository>>,
}

impl AppState {
    /// Constructs application state without hidden global dependencies.
    #[must_use]
    pub fn new(
        storage: Arc<dyn ObjectStore>,
        metadata: Arc<dyn MetadataRepository>,
        services: Services,
        credentials: Arc<CredentialManager>,
        audit: Arc<dyn AuditRepository>,
        owner: OrganizationId,
        version: &'static str,
    ) -> Self {
        let management_auth = ManagementAuth::legacy_root(Arc::clone(&credentials));
        Self {
            storage,
            metadata,
            services,
            credentials,
            audit,
            owner,
            version,
            management_auth,
            events: None,
        }
    }

    /// Replaces legacy root Basic authentication with dedicated management tokens.
    #[must_use]
    pub fn with_management_auth(mut self, management_auth: ManagementAuth) -> Self {
        self.management_auth = management_auth;
        self
    }

    /// Adds durable storage events and webhook administration.
    #[must_use]
    pub fn with_events(mut self, events: Arc<dyn EventRepository>) -> Self {
        self.events = Some(events);
        self
    }

    async fn check_ready(&self) -> Result<(), ReadinessError> {
        tokio::try_join!(
            async {
                self.storage
                    .check_ready()
                    .await
                    .map_err(ReadinessError::Storage)
            },
            async {
                self.metadata
                    .check_ready()
                    .await
                    .map_err(ReadinessError::Metadata)
            },
            async {
                self.audit
                    .check_ready()
                    .await
                    .map_err(ReadinessError::Audit)
            },
            async {
                if let Some(events) = &self.events {
                    events.check_ready().await.map_err(ReadinessError::Events)?;
                }
                Ok(())
            },
        )?;
        Ok(())
    }
}

/// Coarse management roles kept separate from S3 policy actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagementRole {
    /// Full credential, policy, storage, and audit administration.
    SystemAdministrator,
    /// Bucket, object, quota, repair, and integrity administration.
    StorageAdministrator,
    /// Read-only access to operational metadata and the audit trail.
    Auditor,
}

#[derive(Clone)]
struct ManagementToken {
    digest: [u8; 32],
    role: ManagementRole,
}

/// Dedicated bearer-token authentication for the native management plane.
#[derive(Clone)]
pub struct ManagementAuth {
    tokens: Arc<[ManagementToken]>,
    legacy_root: Option<Arc<CredentialManager>>,
}

impl ManagementAuth {
    /// Creates a token set. At least the system-administrator token is expected
    /// for a production deployment; optional role tokens can be omitted.
    #[must_use]
    pub fn bearer_tokens(
        system_administrator: &[u8],
        storage_administrator: Option<&[u8]>,
        auditor: Option<&[u8]>,
    ) -> Self {
        let mut tokens = vec![ManagementToken::new(
            system_administrator,
            ManagementRole::SystemAdministrator,
        )];
        if let Some(token) = storage_administrator {
            tokens.push(ManagementToken::new(
                token,
                ManagementRole::StorageAdministrator,
            ));
        }
        if let Some(token) = auditor {
            tokens.push(ManagementToken::new(token, ManagementRole::Auditor));
        }
        Self {
            tokens: tokens.into(),
            legacy_root: None,
        }
    }

    fn legacy_root(credentials: Arc<CredentialManager>) -> Self {
        Self {
            tokens: Arc::from([]),
            legacy_root: Some(credentials),
        }
    }

    fn authenticate(&self, request: &Request) -> Option<ManagementPrincipal> {
        let authorization = request
            .headers()
            .get(header::AUTHORIZATION)?
            .to_str()
            .ok()?;
        if let Some(token) = authorization.strip_prefix("Bearer ") {
            let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
            return self.tokens.iter().find_map(|candidate| {
                bool::from(candidate.digest.ct_eq(&digest)).then_some(ManagementPrincipal {
                    role: candidate.role,
                })
            });
        }
        let credentials = self.legacy_root.as_ref()?;
        let encoded = authorization.strip_prefix("Basic ")?;
        let decoded = STANDARD.decode(encoded).ok()?;
        let delimiter = decoded.iter().position(|byte| *byte == b':')?;
        let access = std::str::from_utf8(&decoded[..delimiter]).ok()?;
        credentials
            .verify_root(access, &decoded[delimiter + 1..])
            .then_some(ManagementPrincipal {
                role: ManagementRole::SystemAdministrator,
            })
    }
}

impl ManagementToken {
    fn new(token: &[u8], role: ManagementRole) -> Self {
        Self {
            digest: Sha256::digest(token).into(),
            role,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ManagementPrincipal {
    role: ManagementRole,
}

impl ManagementPrincipal {
    fn permits(self, request: &Request) -> bool {
        let path = request.uri().path();
        match self.role {
            ManagementRole::SystemAdministrator => true,
            ManagementRole::StorageAdministrator => {
                !path.starts_with("/api/v1/service-accounts")
                    && !path.starts_with("/api/v1/policies")
                    && !path.starts_with("/api/v1/audit")
                    && !path.starts_with("/api/v1/webhooks")
                    && !path.starts_with("/api/v1/webhook-deliveries")
            }
            ManagementRole::Auditor => {
                request.method() == axum::http::Method::GET
                    && (path == "/api/v1/audit/events"
                        || path == "/api/v1/storage/status"
                        || path == "/api/v1/storage/usage"
                        || path == "/api/v1/storage/inspect"
                        || path == "/api/v1/buckets"
                        || path == "/api/v1/webhooks"
                        || path == "/api/v1/webhook-deliveries")
            }
        }
    }

    const fn audit_name(self) -> &'static str {
        match self.role {
            ManagementRole::SystemAdministrator => "management:system-administrator",
            ManagementRole::StorageAdministrator => "management:storage-administrator",
            ManagementRole::Auditor => "management:auditor",
        }
    }
}

/// Builds public operational routes and authenticated administrative routes.
pub fn router(state: AppState) -> Router {
    let administrative = Router::new()
        .route("/api/v1/storage/status", get(storage_status))
        .route("/api/v1/storage/usage", get(storage_usage))
        .route("/api/v1/storage/inspect", get(storage_inspect))
        .route(
            "/api/v1/storage/repair",
            axum::routing::post(storage_repair),
        )
        .route("/api/v1/audit/events", get(list_audit_events))
        .route("/api/v1/webhooks", get(list_webhooks).post(create_webhook))
        .route("/api/v1/webhooks/{id}", delete(delete_webhook))
        .route(
            "/api/v1/webhooks/{id}/status",
            axum::routing::put(set_webhook_status),
        )
        .route("/api/v1/webhook-deliveries", get(list_webhook_deliveries))
        .route("/api/v1/buckets", get(list_buckets).post(create_bucket))
        .route("/api/v1/buckets/{bucket}", delete(delete_bucket))
        .route(
            "/api/v1/buckets/{bucket}/versioning",
            get(get_bucket_versioning).put(set_bucket_versioning),
        )
        .route(
            "/api/v1/buckets/{bucket}/quota",
            axum::routing::put(set_bucket_quota),
        )
        .route(
            "/api/v1/buckets/{bucket}/lifecycle",
            get(list_lifecycle_rules).post(create_lifecycle_rule),
        )
        .route(
            "/api/v1/lifecycle-rules/{id}",
            delete(delete_lifecycle_rule),
        )
        .route(
            "/api/v1/service-accounts",
            get(list_service_accounts).post(create_service_account),
        )
        .route(
            "/api/v1/service-accounts/{id}",
            get(get_service_account).delete(delete_service_account),
        )
        .route(
            "/api/v1/service-accounts/{id}/status",
            axum::routing::put(set_service_account_status),
        )
        .route(
            "/api/v1/service-accounts/{id}/credentials",
            axum::routing::post(rotate_credential),
        )
        .route(
            "/api/v1/service-accounts/{id}/temporary-credentials",
            axum::routing::post(issue_temporary_credential),
        )
        .route(
            "/api/v1/service-accounts/{id}/credentials/{credential_id}/status",
            axum::routing::put(set_credential_status),
        )
        .route("/api/v1/policies", get(list_policies).post(create_policy))
        .route(
            "/api/v1/policies/{policy_id}/bindings/{account_id}",
            axum::routing::put(attach_policy).delete(detach_policy),
        )
        .route(
            "/api/v1/restore/{bucket}/{*key}",
            axum::routing::post(restore_version),
        )
        .route(
            "/api/v1/verify/objects/{bucket}/{*key}",
            axum::routing::post(verify_object),
        )
        .route(
            "/api/v1/verify/buckets/{bucket}",
            axum::routing::post(verify_bucket),
        )
        .route_layer(middleware::from_fn_with_state(
            state.management_auth.clone(),
            require_management,
        ));

    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/api/v1/system/info", get(system_info))
        .route("/metrics", get(metrics))
        .merge(administrative)
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            request_context,
        ))
        .with_state(state)
}

/// Serves until shutdown is requested, then drains requests up to `grace_period`.
pub async fn serve<F>(
    listener: TcpListener,
    application: Router,
    shutdown: F,
    grace_period: Duration,
) -> Result<(), ServerError>
where
    F: Future<Output = ()> + Send + 'static,
{
    let cancellation = CancellationToken::new();
    let graceful = axum::serve(
        listener,
        application.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(cancellation.clone().cancelled_owned())
    .into_future();
    tokio::pin!(graceful);
    tokio::select! {
        result = &mut graceful => result.map_err(ServerError::Serve),
        () = shutdown => {
            cancellation.cancel();
            match timeout(grace_period, &mut graceful).await {
                Ok(result) => result.map_err(ServerError::Serve),
                Err(_) => Err(ServerError::ShutdownTimeout(grace_period)),
            }
        }
    }
}

async fn require_management(
    State(authentication): State<ManagementAuth>,
    mut request: Request,
    next: Next,
) -> Response {
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .cloned()
        .unwrap_or_else(RequestId::new);
    let Some(principal) = authentication.authenticate(&request) else {
        return ApiError::unauthorized(request_id).into_response();
    };
    if !principal.permits(&request) {
        return ApiError::forbidden(request_id).into_response();
    }
    request.extensions_mut().insert(principal);
    let mut response = next.run(request).await;
    response.extensions_mut().insert(principal);
    response
}

async fn health() -> Json<StatusResponse> {
    Json(StatusResponse { status: "ok" })
}

async fn ready(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<StatusResponse>, ApiError> {
    ensure_ready(&state, request_id).await?;
    Ok(Json(StatusResponse { status: "ready" }))
}

async fn system_info(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<SystemInfoResponse>, ApiError> {
    ensure_ready(&state, request_id).await?;
    Ok(Json(SystemInfoResponse {
        name: "oes",
        version: state.version,
        status: "ready",
    }))
}

async fn storage_status(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<StorageStatusResponse>, ApiError> {
    state
        .services
        .objects
        .status()
        .await
        .map(StorageStatusResponse::from)
        .map(Json)
        .map_err(|error| internal_service_error(error, request_id))
}

async fn storage_usage(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<StorageUsage>, ApiError> {
    state
        .services
        .objects
        .usage()
        .await
        .map(Json)
        .map_err(|error| internal_service_error(error, request_id))
}

#[derive(Debug, Deserialize)]
struct StorageInspectionQuery {
    #[serde(default = "default_inspection_limit")]
    maximum_entries: usize,
}

const fn default_inspection_limit() -> usize {
    100_000
}

async fn storage_inspect(
    State(state): State<AppState>,
    Query(query): Query<StorageInspectionQuery>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<StorageInspection>, ApiError> {
    state
        .services
        .objects
        .inspect(query.maximum_entries)
        .await
        .map(Json)
        .map_err(|error| service_to_api_error(error, request_id))
}

#[derive(Debug, Deserialize)]
struct StorageRepairInput {
    #[serde(default = "default_inspection_limit")]
    maximum_entries: usize,
    #[serde(default = "dry_run_by_default")]
    dry_run: bool,
}

const fn dry_run_by_default() -> bool {
    true
}

async fn storage_repair(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<StorageRepairInput>,
) -> Result<Json<StorageRepairResult>, ApiError> {
    state
        .services
        .objects
        .repair(StorageRepairRequest {
            maximum_entries: input.maximum_entries,
            dry_run: input.dry_run,
        })
        .await
        .map(Json)
        .map_err(|error| service_to_api_error(error, request_id))
}

async fn list_buckets(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<Vec<Bucket>>, ApiError> {
    state
        .services
        .buckets
        .list()
        .await
        .map(Json)
        .map_err(|error| internal_service_error(error, request_id))
}

#[derive(Deserialize)]
struct CreateBucketRequest {
    name: String,
}

async fn create_bucket(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<CreateBucketRequest>,
) -> Result<(StatusCode, Json<Bucket>), ApiError> {
    let name = BucketName::new(input.name).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_BUCKET_NAME",
            "Invalid bucket name",
        )
    })?;
    state
        .services
        .buckets
        .create(name)
        .await
        .map(|bucket| (StatusCode::CREATED, Json(bucket)))
        .map_err(|error| service_to_api_error(error, request_id))
}

async fn delete_bucket(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<StatusCode, ApiError> {
    let name = BucketName::new(bucket).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_BUCKET_NAME",
            "Invalid bucket name",
        )
    })?;
    state
        .services
        .buckets
        .delete(&name)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|error| service_to_api_error(error, request_id))
}

async fn list_service_accounts(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<Vec<ServiceAccountInfo>>, ApiError> {
    state
        .credentials
        .list_service_accounts()
        .await
        .map(Json)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "credential operation failed");
            ApiError::internal(request_id)
        })
}

#[derive(Deserialize)]
struct CreateServiceAccountRequest {
    name: String,
    #[serde(default)]
    description: String,
}

#[derive(Serialize)]
struct IssuedServiceAccountResponse {
    account: oes_auth::ServiceAccount,
    credential: oes_auth::Credential,
    secret_access_key: String,
}

async fn create_service_account(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<CreateServiceAccountRequest>,
) -> Result<(StatusCode, Json<IssuedServiceAccountResponse>), ApiError> {
    let issued = state
        .credentials
        .create_service_account_with_description(input.name, input.description, state.owner)
        .await
        .map_err(|error| {
            error!(%error, request_id = %request_id, "credential issuance failed");
            ApiError::bad_request(
                request_id.clone(),
                "INVALID_SERVICE_ACCOUNT",
                "Invalid service account",
            )
        })?;
    let secret_access_key =
        String::from_utf8(issued.secret.expose().to_vec()).map_err(|error| {
            error!(%error, request_id = %request_id, "generated credential was not UTF-8");
            ApiError::internal(request_id)
        })?;
    Ok((
        StatusCode::CREATED,
        Json(IssuedServiceAccountResponse {
            account: issued.info.account,
            credential: issued.info.credential,
            secret_access_key,
        }),
    ))
}

async fn get_service_account(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<ServiceAccountInfo>, ApiError> {
    let id = ServiceAccountId::from_str(&id).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_SERVICE_ACCOUNT_ID",
            "Invalid service account ID",
        )
    })?;
    state
        .credentials
        .get_service_account(id)
        .await
        .map(Json)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "service account lookup failed");
            ApiError::internal(request_id)
        })
}

async fn delete_service_account(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<StatusCode, ApiError> {
    let id = parse_service_account_id(&id, &request_id)?;
    state
        .credentials
        .delete_service_account(id)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "service account deletion failed");
            ApiError::internal(request_id)
        })
}

#[derive(Deserialize)]
struct StatusChangeRequest {
    enabled: bool,
}

async fn set_service_account_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<StatusChangeRequest>,
) -> Result<Json<ServiceAccountInfo>, ApiError> {
    let id = parse_service_account_id(&id, &request_id)?;
    state
        .credentials
        .set_service_account_enabled(id, input.enabled)
        .await
        .map(Json)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "service account status update failed");
            ApiError::internal(request_id)
        })
}

#[derive(Deserialize)]
struct RotateCredentialRequest {
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

async fn rotate_credential(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<RotateCredentialRequest>,
) -> Result<(StatusCode, Json<IssuedServiceAccountResponse>), ApiError> {
    let id = parse_service_account_id(&id, &request_id)?;
    let issued = state
        .credentials
        .rotate_credential(id, input.expires_at)
        .await
        .map_err(|error| {
            error!(%error, request_id = %request_id, "credential rotation failed");
            ApiError::internal(request_id.clone())
        })?;
    let secret_access_key = String::from_utf8(issued.secret.expose().to_vec())
        .map_err(|_| ApiError::internal(request_id))?;
    Ok((
        StatusCode::CREATED,
        Json(IssuedServiceAccountResponse {
            account: issued.info.account,
            credential: issued.info.credential,
            secret_access_key,
        }),
    ))
}

#[derive(Debug, Deserialize)]
struct TemporaryCredentialRequest {
    expires_in_seconds: u64,
}

async fn issue_temporary_credential(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<TemporaryCredentialRequest>,
) -> Result<(StatusCode, Json<IssuedServiceAccountResponse>), ApiError> {
    if !(60..=86_400).contains(&input.expires_in_seconds) {
        return Err(ApiError::bad_request(
            request_id,
            "INVALID_EXPIRATION",
            "Temporary credentials must expire between 60 and 86400 seconds",
        ));
    }
    let id = parse_service_account_id(&id, &request_id)?;
    let seconds = i64::try_from(input.expires_in_seconds).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_EXPIRATION",
            "Temporary credential expiration is invalid",
        )
    })?;
    let expires_at = chrono::Utc::now() + chrono::Duration::seconds(seconds);
    let issued = state
        .credentials
        .rotate_credential(id, Some(expires_at))
        .await
        .map_err(|error| {
            error!(%error, request_id = %request_id, "temporary credential issuance failed");
            ApiError::internal(request_id.clone())
        })?;
    let secret_access_key = String::from_utf8(issued.secret.expose().to_vec())
        .map_err(|_| ApiError::internal(request_id))?;
    Ok((
        StatusCode::CREATED,
        Json(IssuedServiceAccountResponse {
            account: issued.info.account,
            credential: issued.info.credential,
            secret_access_key,
        }),
    ))
}

async fn set_credential_status(
    State(state): State<AppState>,
    Path((id, credential_id)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<StatusChangeRequest>,
) -> Result<StatusCode, ApiError> {
    let id = parse_service_account_id(&id, &request_id)?;
    let credential_id = Uuid::parse_str(&credential_id).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_CREDENTIAL_ID",
            "Invalid credential ID",
        )
    })?;
    state
        .credentials
        .set_credential_enabled(id, credential_id, input.enabled)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "credential status update failed");
            ApiError::internal(request_id)
        })
}

fn parse_service_account_id(
    value: &str,
    request_id: &RequestId,
) -> Result<ServiceAccountId, ApiError> {
    ServiceAccountId::from_str(value).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_SERVICE_ACCOUNT_ID",
            "Invalid service account ID",
        )
    })
}

async fn get_bucket_versioning(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<VersioningResponse>, ApiError> {
    let name = BucketName::new(bucket).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_BUCKET_NAME",
            "Invalid bucket name",
        )
    })?;
    let bucket = state
        .services
        .buckets
        .head(&name)
        .await
        .map_err(|error| service_to_api_error(error, request_id))?;
    Ok(Json(VersioningResponse {
        versioning: bucket.versioning,
    }))
}

#[derive(Serialize)]
struct VersioningResponse {
    versioning: VersioningState,
}

#[derive(Deserialize)]
struct SetVersioningRequest {
    versioning: VersioningState,
}

async fn set_bucket_versioning(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<SetVersioningRequest>,
) -> Result<Json<Bucket>, ApiError> {
    let name = BucketName::new(bucket).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_BUCKET_NAME",
            "Invalid bucket name",
        )
    })?;
    state
        .services
        .buckets
        .set_versioning(&name, input.versioning)
        .await
        .map(Json)
        .map_err(|error| service_to_api_error(error, request_id))
}

#[derive(Deserialize)]
struct SetQuotaRequest {
    quota: BucketQuota,
}

async fn set_bucket_quota(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<SetQuotaRequest>,
) -> Result<Json<Bucket>, ApiError> {
    let name = BucketName::new(bucket).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_BUCKET_NAME",
            "Invalid bucket name",
        )
    })?;
    state
        .services
        .buckets
        .set_quota(&name, input.quota)
        .await
        .map(Json)
        .map_err(|error| service_to_api_error(error, request_id))
}

#[derive(Debug, Deserialize)]
struct CreateLifecycleRuleRequest {
    #[serde(default)]
    prefix: String,
    #[serde(default = "enabled_by_default")]
    enabled: bool,
    expiration: Option<ExpirationDays>,
    noncurrent_version_expiration: Option<ExpirationDays>,
}

const fn enabled_by_default() -> bool {
    true
}

async fn create_lifecycle_rule(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<CreateLifecycleRuleRequest>,
) -> Result<(StatusCode, Json<LifecycleRule>), ApiError> {
    let name = BucketName::new(bucket).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_BUCKET_NAME",
            "Invalid bucket name",
        )
    })?;
    let bucket = state
        .services
        .buckets
        .head(&name)
        .await
        .map_err(|error| service_to_api_error(error, request_id.clone()))?;
    let now = chrono::Utc::now();
    let rule = LifecycleRule {
        id: LifecycleRuleId::new(),
        bucket_id: bucket.id,
        prefix: input.prefix,
        enabled: input.enabled,
        expiration: input.expiration,
        noncurrent_version_expiration: input.noncurrent_version_expiration,
        created_at: now,
        updated_at: now,
    };
    rule.validate().map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_LIFECYCLE_RULE",
            "Lifecycle rule is invalid",
        )
    })?;
    state
        .metadata
        .put_lifecycle_rule(&rule)
        .await
        .map_err(|error| {
            error!(%error, request_id = %request_id, "lifecycle rule creation failed");
            ApiError::internal(request_id)
        })?;
    Ok((StatusCode::CREATED, Json(rule)))
}

async fn list_lifecycle_rules(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<Vec<LifecycleRule>>, ApiError> {
    let name = BucketName::new(bucket).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_BUCKET_NAME",
            "Invalid bucket name",
        )
    })?;
    let bucket = state
        .services
        .buckets
        .head(&name)
        .await
        .map_err(|error| service_to_api_error(error, request_id.clone()))?;
    state
        .metadata
        .list_lifecycle_rules(Some(bucket.id))
        .await
        .map(Json)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "lifecycle rule listing failed");
            ApiError::internal(request_id)
        })
}

async fn delete_lifecycle_rule(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<StatusCode, ApiError> {
    let id = LifecycleRuleId::from_str(&id).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_LIFECYCLE_RULE_ID",
            "Invalid lifecycle rule ID",
        )
    })?;
    state
        .metadata
        .delete_lifecycle_rule(id)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "lifecycle rule deletion failed");
            ApiError::bad_request(
                request_id,
                "LIFECYCLE_RULE_NOT_FOUND",
                "Lifecycle rule was not found",
            )
        })
}

#[derive(Deserialize)]
struct CreatePolicyRequest {
    name: String,
    #[serde(default)]
    description: String,
    statements: Vec<PolicyStatement>,
}

async fn create_policy(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<CreatePolicyRequest>,
) -> Result<(StatusCode, Json<Policy>), ApiError> {
    state
        .credentials
        .create_policy(input.name, input.description, input.statements)
        .await
        .map(|policy| (StatusCode::CREATED, Json(policy)))
        .map_err(|error| {
            error!(%error, request_id = %request_id, "policy creation failed");
            ApiError::bad_request(request_id, "INVALID_POLICY", "Invalid policy")
        })
}

async fn list_policies(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<Vec<Policy>>, ApiError> {
    state
        .credentials
        .list_policies()
        .await
        .map(Json)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "policy listing failed");
            ApiError::internal(request_id)
        })
}

async fn attach_policy(
    State(state): State<AppState>,
    Path((policy_id, account_id)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
) -> Result<StatusCode, ApiError> {
    let policy_id = Uuid::parse_str(&policy_id).map_err(|_| {
        ApiError::bad_request(request_id.clone(), "INVALID_POLICY_ID", "Invalid policy ID")
    })?;
    let account_id = parse_service_account_id(&account_id, &request_id)?;
    state
        .credentials
        .attach_policy(account_id, policy_id)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "policy attachment failed");
            ApiError::internal(request_id)
        })
}

async fn detach_policy(
    State(state): State<AppState>,
    Path((policy_id, account_id)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
) -> Result<StatusCode, ApiError> {
    let policy_id = Uuid::parse_str(&policy_id).map_err(|_| {
        ApiError::bad_request(request_id.clone(), "INVALID_POLICY_ID", "Invalid policy ID")
    })?;
    let account_id = parse_service_account_id(&account_id, &request_id)?;
    state
        .credentials
        .detach_policy(account_id, policy_id)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "policy detachment failed");
            ApiError::internal(request_id)
        })
}

#[derive(Deserialize)]
struct RestoreVersionRequest {
    version_id: VersionId,
}

async fn restore_version(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<RestoreVersionRequest>,
) -> Result<(StatusCode, Json<ObjectMetadata>), ApiError> {
    let bucket = BucketName::new(bucket).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_BUCKET_NAME",
            "Invalid bucket name",
        )
    })?;
    let key = ObjectKey::new(key).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_OBJECT_KEY",
            "Invalid object key",
        )
    })?;
    state
        .services
        .objects
        .restore_version(&bucket, key, input.version_id)
        .await
        .map(|result| (StatusCode::CREATED, Json(result.metadata)))
        .map_err(|error| service_to_api_error(error, request_id))
}

async fn verify_object(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<ObjectMetadata>, ApiError> {
    let bucket = BucketName::new(bucket).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_BUCKET_NAME",
            "Invalid bucket name",
        )
    })?;
    let key = ObjectKey::new(key).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_OBJECT_KEY",
            "Invalid object key",
        )
    })?;
    state
        .services
        .objects
        .verify(&bucket, key)
        .await
        .map(Json)
        .map_err(|error| service_to_api_error(error, request_id))
}

#[derive(Serialize)]
struct VerifyBucketResponse {
    verified_objects: u64,
    failures: u64,
}

async fn verify_bucket(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<VerifyBucketResponse>, ApiError> {
    let bucket = BucketName::new(bucket).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_BUCKET_NAME",
            "Invalid bucket name",
        )
    })?;
    let mut marker = None;
    let mut verified = 0_u64;
    let mut failures = 0_u64;
    loop {
        let page = state
            .services
            .objects
            .list(ServiceListRequest {
                bucket: bucket.clone(),
                prefix: String::new(),
                delimiter: None,
                maximum_keys: 1_000,
                start_after: marker.clone(),
            })
            .await
            .map_err(|error| service_to_api_error(error, request_id.clone()))?;
        for object in &page.objects {
            if state
                .services
                .objects
                .verify(&bucket, object.key.clone())
                .await
                .is_ok()
            {
                verified += 1;
            } else {
                failures += 1;
            }
        }
        if !page.is_truncated {
            break;
        }
        marker = page.next_marker;
    }
    Ok(Json(VerifyBucketResponse {
        verified_objects: verified,
        failures,
    }))
}

async fn metrics(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Response, ApiError> {
    let metrics = state.services.metrics.snapshot();
    let usage = state
        .services
        .objects
        .usage()
        .await
        .map_err(|error| internal_service_error(error, request_id))?;
    let body = format!(
        concat!(
            "# TYPE oes_s3_requests_total counter\n",
            "oes_s3_requests_total {}\n",
            "# TYPE oes_requests_total counter\n",
            "oes_requests_total {}\n",
            "# TYPE oes_errors_total counter\n",
            "oes_errors_total {}\n",
            "# TYPE oes_objects_total gauge\n",
            "oes_objects_total {}\n",
            "# TYPE oes_storage_bytes gauge\n",
            "oes_storage_bytes {}\n",
            "# TYPE oes_versions_total gauge\n",
            "oes_versions_total {}\n",
            "# TYPE oes_buckets_total gauge\n",
            "oes_buckets_total {}\n",
            "# TYPE oes_storage_logical_bytes gauge\n",
            "oes_storage_logical_bytes {}\n",
            "# TYPE oes_storage_physical_bytes gauge\n",
            "oes_storage_physical_bytes {}\n",
            "# TYPE oes_multipart_bytes gauge\n",
            "oes_multipart_bytes {}\n",
            "# TYPE oes_upload_bytes_total counter\n",
            "oes_upload_bytes_total {}\n",
            "# TYPE oes_download_bytes_total counter\n",
            "oes_download_bytes_total {}\n"
        ),
        metrics.requests,
        metrics.requests,
        metrics.errors,
        usage.object_count,
        usage.bytes_used,
        usage.version_count,
        usage.bucket_count,
        usage.bytes_used,
        usage.physical_bytes,
        usage.temporary_multipart_bytes,
        metrics.upload_bytes,
        metrics.download_bytes,
    );
    Ok(([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], body).into_response())
}

async fn create_webhook(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<CreateWebhookRequest>,
) -> Result<(StatusCode, Json<CreatedWebhook>), ApiError> {
    event_repository(&state, &request_id)?
        .create_webhook(input)
        .await
        .map(|created| (StatusCode::CREATED, Json(created)))
        .map_err(|error| {
            error!(%error, request_id = %request_id, "webhook creation failed");
            ApiError::bad_request(
                request_id,
                "INVALID_WEBHOOK",
                "Webhook configuration is invalid or disallowed",
            )
        })
}

async fn list_webhooks(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<Vec<WebhookSubscription>>, ApiError> {
    event_repository(&state, &request_id)?
        .list_webhooks()
        .await
        .map(Json)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "webhook listing failed");
            ApiError::internal(request_id)
        })
}

#[derive(Debug, Deserialize)]
struct WebhookStatusRequest {
    enabled: bool,
}

async fn set_webhook_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<WebhookStatusRequest>,
) -> Result<Json<WebhookSubscription>, ApiError> {
    let id = WebhookId::from_str(&id).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_WEBHOOK_ID",
            "Invalid webhook ID",
        )
    })?;
    event_repository(&state, &request_id)?
        .set_webhook_enabled(id, input.enabled)
        .await
        .map(Json)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "webhook status update failed");
            ApiError::bad_request(request_id, "WEBHOOK_NOT_FOUND", "Webhook was not found")
        })
}

async fn delete_webhook(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<StatusCode, ApiError> {
    let id = WebhookId::from_str(&id).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_WEBHOOK_ID",
            "Invalid webhook ID",
        )
    })?;
    event_repository(&state, &request_id)?
        .delete_webhook(id)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "webhook deletion failed");
            ApiError::bad_request(request_id, "WEBHOOK_NOT_FOUND", "Webhook was not found")
        })
}

#[derive(Debug, Deserialize)]
struct DeliveryLogQuery {
    #[serde(default = "default_delivery_limit")]
    limit: usize,
}

const fn default_delivery_limit() -> usize {
    100
}

async fn list_webhook_deliveries(
    State(state): State<AppState>,
    Query(query): Query<DeliveryLogQuery>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<Vec<WebhookDeliveryLog>>, ApiError> {
    event_repository(&state, &request_id)?
        .list_delivery_logs(query.limit)
        .await
        .map(Json)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "webhook delivery log query failed");
            ApiError::bad_request(
                request_id,
                "INVALID_DELIVERY_QUERY",
                "Delivery query is invalid",
            )
        })
}

fn event_repository<'a>(
    state: &'a AppState,
    request_id: &RequestId,
) -> Result<&'a Arc<dyn EventRepository>, ApiError> {
    state
        .events
        .as_ref()
        .ok_or_else(|| ApiError::service_unavailable(request_id.clone()))
}

#[derive(Debug, Deserialize)]
struct AuditQueryParameters {
    since: Option<chrono::DateTime<chrono::Utc>>,
    until: Option<chrono::DateTime<chrono::Utc>>,
    principal: Option<String>,
    operation: Option<String>,
    resource: Option<String>,
    result: Option<AuditResult>,
    after_time: Option<chrono::DateTime<chrono::Utc>>,
    after_id: Option<oes_core::AuditEventId>,
    #[serde(default = "default_audit_limit")]
    limit: usize,
}

const fn default_audit_limit() -> usize {
    100
}

#[derive(Serialize)]
struct AuditEventsResponse {
    events: Vec<AuditEvent>,
    next_time: Option<chrono::DateTime<chrono::Utc>>,
    next_id: Option<oes_core::AuditEventId>,
}

async fn list_audit_events(
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

async fn ensure_ready(state: &AppState, request_id: RequestId) -> Result<(), ApiError> {
    if let Err(error_value) = state.check_ready().await {
        error!(request_id = %request_id, error = %error_value, "readiness check failed");
        return Err(ApiError::service_unavailable(request_id));
    }
    Ok(())
}

async fn not_found(Extension(request_id): Extension<RequestId>) -> ApiError {
    ApiError::not_found(request_id)
}

async fn request_context(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let request_id = request
        .headers()
        .get(&REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(RequestId::accept)
        .unwrap_or_else(RequestId::new);
    request.extensions_mut().insert(request_id.clone());
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let source_ip = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|connect| connect.0.ip().to_string());
    let started = Instant::now();
    let span =
        info_span!("http.request", request_id = %request_id, method = %method, route = %path);
    let mut response = next.run(request).instrument(span.clone()).await;
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    info!(parent: &span, status = response.status().as_u16(), latency_ms, "request completed");
    if let Ok(value) = HeaderValue::from_str(request_id.as_str()) {
        response
            .headers_mut()
            .insert(REQUEST_ID_HEADER.clone(), value);
    }
    if path.starts_with("/api/v1/") {
        let result = match response.status() {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => AuditResult::Denied,
            status if status.is_success() => AuditResult::Success,
            _ => AuditResult::Failure,
        };
        let event = AuditEvent {
            event_id: oes_core::AuditEventId::new(),
            timestamp: chrono::Utc::now(),
            request_id: Some(request_id.to_string()),
            principal: response
                .extensions()
                .get::<ManagementPrincipal>()
                .copied()
                .map_or(
                    "management:unauthenticated",
                    ManagementPrincipal::audit_name,
                )
                .into(),
            credential_id: None,
            source_ip,
            operation: format!("{} {}", method, path),
            resource: path,
            result,
            metadata: Default::default(),
        };
        if let Err(error) = state.audit.append(&event).await {
            error!(%error, request_id = %request_id, "durable audit append failed");
        }
    }
    response
}

/// A validated request correlation identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestId(String);

impl RequestId {
    fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    fn accept(value: &str) -> Option<Self> {
        (!value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte)))
        .then(|| Self(value.to_owned()))
    }

    /// Returns the validated identifier text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for RequestId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    status: &'static str,
}

#[derive(Debug, Serialize)]
struct SystemInfoResponse {
    name: &'static str,
    version: &'static str,
    status: &'static str,
}

#[derive(Debug, Serialize)]
struct StorageStatusResponse {
    capacity_bytes: u64,
    available_bytes: u64,
    temporary_upload_bytes: u64,
}

impl From<StorageStatus> for StorageStatusResponse {
    fn from(value: StorageStatus) -> Self {
        Self {
            capacity_bytes: value.capacity_bytes,
            available_bytes: value.available_bytes,
            temporary_upload_bytes: value.temporary_upload_bytes,
        }
    }
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    request_id: RequestId,
}

impl ApiError {
    fn not_found(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "ROUTE_NOT_FOUND",
            "The requested route was not found",
            request_id,
        )
    }

    fn unauthorized(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "Authentication is required",
            request_id,
        )
    }

    fn forbidden(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "FORBIDDEN",
            "The management role does not permit this operation",
            request_id,
        )
    }

    fn service_unavailable(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "SERVICE_NOT_READY",
            "The service is not ready",
            request_id,
        )
    }

    fn internal(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "An internal error occurred",
            request_id,
        )
    }

    fn bad_request(request_id: RequestId, code: &'static str, message: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message, request_id)
    }

    const fn new(
        status: StatusCode,
        code: &'static str,
        message: &'static str,
        request_id: RequestId,
    ) -> Self {
        Self {
            status,
            code,
            message,
            request_id,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorEnvelope {
                error: ErrorBody {
                    code: self.code,
                    message: self.message,
                    request_id: self.request_id.as_str(),
                },
            }),
        )
            .into_response()
    }
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope<'a> {
    error: ErrorBody<'a>,
}

#[derive(Debug, Serialize)]
struct ErrorBody<'a> {
    code: &'static str,
    message: &'static str,
    request_id: &'a str,
}

fn service_to_api_error(error: ServiceError, request_id: RequestId) -> ApiError {
    match error {
        ServiceError::BucketNotFound => ApiError::new(
            StatusCode::NOT_FOUND,
            "BUCKET_NOT_FOUND",
            "Bucket was not found",
            request_id,
        ),
        ServiceError::BucketAlreadyExists => ApiError::new(
            StatusCode::CONFLICT,
            "BUCKET_ALREADY_EXISTS",
            "Bucket already exists",
            request_id,
        ),
        ServiceError::BucketNotEmpty => ApiError::new(
            StatusCode::CONFLICT,
            "BUCKET_NOT_EMPTY",
            "Bucket is not empty",
            request_id,
        ),
        ServiceError::QuotaExceeded => ApiError::new(
            StatusCode::INSUFFICIENT_STORAGE,
            "QUOTA_EXCEEDED",
            "The storage quota would be exceeded",
            request_id,
        ),
        ServiceError::Core(_)
        | ServiceError::InvalidRequest(_)
        | ServiceError::MetadataTooLarge => {
            ApiError::bad_request(request_id, "INVALID_REQUEST", "Invalid request")
        }
        error => internal_service_error(error, request_id),
    }
}

fn internal_service_error(error_value: ServiceError, request_id: RequestId) -> ApiError {
    error!(request_id = %request_id, error = %error_value, "management operation failed");
    ApiError::internal(request_id)
}

#[derive(Debug, Error)]
enum ReadinessError {
    #[error("storage dependency failed: {0}")]
    Storage(oes_storage::StorageError),
    #[error("metadata dependency failed: {0}")]
    Metadata(oes_metadata::MetadataError),
    #[error("audit dependency failed: {0}")]
    Audit(oes_audit::AuditError),
    #[error("event dependency failed: {0}")]
    Events(oes_events::EventError),
}

/// HTTP serving and bounded-shutdown failures.
#[derive(Debug, Error)]
pub enum ServerError {
    /// The HTTP listener failed.
    #[error("HTTP server failed: {0}")]
    Serve(#[source] std::io::Error),
    /// In-flight requests did not finish within the configured interval.
    #[error("graceful shutdown exceeded {0:?}")]
    ShutdownTimeout(Duration),
}

#[cfg(test)]
mod tests {
    use http_body_util::BodyExt;

    use super::*;

    #[test]
    fn incoming_request_ids_are_strictly_validated() {
        assert!(RequestId::accept("trace-123.example").is_some());
        assert!(RequestId::accept("").is_none());
        assert!(RequestId::accept("contains a space").is_none());
        assert!(RequestId::accept(&"a".repeat(129)).is_none());
    }

    #[tokio::test]
    async fn errors_use_the_stable_json_envelope() {
        let response = ApiError::not_found(RequestId("request-1".into())).into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
        assert_eq!(value["error"]["code"], "ROUTE_NOT_FOUND");
        assert_eq!(value["error"]["request_id"], "request-1");
    }
}
