//! Native operational and authenticated management HTTP API.

use std::{
    fmt::{self, Display, Formatter},
    future::{Future, IntoFuture},
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Extension, Path, Request, State},
    http::{HeaderValue, StatusCode, header, header::HeaderName},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use oes_auth::{CredentialManager, ServiceAccountInfo};
use oes_core::{Bucket, BucketName, OrganizationId, ServiceAccountId, StorageUsage};
use oes_metadata::MetadataRepository;
use oes_service::{ServiceError, Services};
use oes_storage::{ObjectStore, StorageStatus};
use serde::{Deserialize, Serialize};
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
    owner: OrganizationId,
    version: &'static str,
}

impl AppState {
    /// Constructs application state without hidden global dependencies.
    #[must_use]
    pub fn new(
        storage: Arc<dyn ObjectStore>,
        metadata: Arc<dyn MetadataRepository>,
        services: Services,
        credentials: Arc<CredentialManager>,
        owner: OrganizationId,
        version: &'static str,
    ) -> Self {
        Self {
            storage,
            metadata,
            services,
            credentials,
            owner,
            version,
        }
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
            }
        )?;
        Ok(())
    }
}

/// Builds public operational routes and authenticated administrative routes.
pub fn router(state: AppState) -> Router {
    let administrative = Router::new()
        .route("/api/v1/storage/status", get(storage_status))
        .route("/api/v1/storage/usage", get(storage_usage))
        .route("/api/v1/buckets", get(list_buckets).post(create_bucket))
        .route("/api/v1/buckets/{bucket}", delete(delete_bucket))
        .route(
            "/api/v1/service-accounts",
            get(list_service_accounts).post(create_service_account),
        )
        .route(
            "/api/v1/service-accounts/{id}",
            delete(revoke_service_account),
        )
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&state.credentials),
            require_root,
        ));

    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/api/v1/system/info", get(system_info))
        .route("/metrics", get(metrics))
        .merge(administrative)
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .layer(middleware::from_fn(request_context))
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
    let graceful = axum::serve(listener, application)
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

async fn require_root(
    State(credentials): State<Arc<CredentialManager>>,
    request: Request,
    next: Next,
) -> Response {
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .cloned()
        .unwrap_or_else(RequestId::new);
    let authenticated = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Basic "))
        .and_then(|value| STANDARD.decode(value).ok())
        .and_then(|decoded| {
            let delimiter = decoded.iter().position(|byte| *byte == b':')?;
            let access = std::str::from_utf8(&decoded[..delimiter]).ok()?;
            Some(credentials.verify_root(access, &decoded[delimiter + 1..]))
        })
        .unwrap_or(false);
    if authenticated {
        next.run(request).await
    } else {
        ApiError::unauthorized(request_id).into_response()
    }
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
        .create_service_account(input.name, state.owner)
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

async fn revoke_service_account(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<StatusCode, ApiError> {
    let id = ServiceAccountId::from_str(&id).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_SERVICE_ACCOUNT_ID",
            "Invalid service account ID",
        )
    })?;
    state
        .credentials
        .revoke_service_account(id)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "credential revocation failed");
            ApiError::internal(request_id)
        })
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
            "# TYPE oes_requests_total counter\n",
            "oes_requests_total {}\n",
            "# TYPE oes_errors_total counter\n",
            "oes_errors_total {}\n",
            "# TYPE oes_objects_total gauge\n",
            "oes_objects_total {}\n",
            "# TYPE oes_storage_bytes gauge\n",
            "oes_storage_bytes {}\n",
            "# TYPE oes_upload_bytes_total counter\n",
            "oes_upload_bytes_total {}\n",
            "# TYPE oes_download_bytes_total counter\n",
            "oes_download_bytes_total {}\n"
        ),
        metrics.requests,
        metrics.errors,
        usage.object_count,
        usage.bytes_used,
        metrics.upload_bytes,
        metrics.download_bytes,
    );
    Ok(([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], body).into_response())
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

async fn request_context(mut request: Request, next: Next) -> Response {
    let request_id = request
        .headers()
        .get(&REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(RequestId::accept)
        .unwrap_or_else(RequestId::new);
    request.extensions_mut().insert(request_id.clone());
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
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
