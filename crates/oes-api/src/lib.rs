//! Operational HTTP API, request context, and graceful serving.

use std::{
    fmt::{self, Display, Formatter},
    future::{Future, IntoFuture},
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Extension, Request, State},
    http::{HeaderValue, StatusCode, header::HeaderName},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use oes_metadata::MetadataRepository;
use oes_storage::ObjectStore;
use serde::Serialize;
use thiserror::Error;
use tokio::{net::TcpListener, time::timeout};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, error, info, info_span};
use uuid::Uuid;

static REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

/// Explicit dependencies shared by HTTP handlers.
#[derive(Clone)]
pub struct AppState {
    storage: Arc<dyn ObjectStore>,
    metadata: Arc<dyn MetadataRepository>,
    version: &'static str,
}

impl AppState {
    /// Constructs application state without hidden global dependencies.
    #[must_use]
    pub const fn new(
        storage: Arc<dyn ObjectStore>,
        metadata: Arc<dyn MetadataRepository>,
        version: &'static str,
    ) -> Self {
        Self {
            storage,
            metadata,
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

/// Builds the complete operational HTTP router.
pub fn router(state: AppState, maximum_request_size: usize) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/api/v1/system/info", get(system_info))
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(maximum_request_size))
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
    let span = info_span!(
        "http.request",
        request_id = %request_id,
        method = %method,
        route = %path,
    );
    let mut response = next.run(request).instrument(span.clone()).await;
    let status = response.status();
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    info!(
        parent: &span,
        status = status.as_u16(),
        latency_ms,
        "request completed"
    );
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
        let valid = !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte));
        valid.then(|| Self(value.to_owned()))
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

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    request_id: RequestId,
}

impl ApiError {
    fn not_found(request_id: RequestId) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "ROUTE_NOT_FOUND",
            message: "The requested route was not found",
            request_id,
        }
    }

    fn service_unavailable(request_id: RequestId) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "SERVICE_NOT_READY",
            message: "The service is not ready",
            request_id,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorEnvelope {
            error: ErrorBody {
                code: self.code,
                message: self.message,
                request_id: self.request_id.as_str(),
            },
        };
        (self.status, Json(body)).into_response()
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
    use axum::response::IntoResponse;
    use http_body_util::BodyExt;

    use super::*;

    #[test]
    fn incoming_request_ids_are_strictly_validated() {
        assert_eq!(
            RequestId::accept("trace-123.example")
                .expect("safe ID")
                .as_str(),
            "trace-123.example"
        );
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
        assert!(!String::from_utf8_lossy(&body).contains('/'));
    }
}
