use std::time::Duration;

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use record_store_service::ServiceError;
use serde::Serialize;
use thiserror::Error;
use tracing::error;

use crate::*;

#[derive(Debug)]
pub(crate) struct ApiError {
    pub(crate) status: StatusCode,
    pub(crate) code: &'static str,
    pub(crate) message: String,
    pub(crate) request_id: RequestId,
}

impl ApiError {
    pub(crate) fn not_found(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "ROUTE_NOT_FOUND",
            "The requested route was not found",
            request_id,
        )
    }

    pub(crate) fn unauthorized(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "Authentication is required",
            request_id,
        )
    }

    pub(crate) fn forbidden(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "FORBIDDEN",
            "The management role does not permit this operation",
            request_id,
        )
    }

    pub(crate) fn service_unavailable(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "SERVICE_NOT_READY",
            "The service is not ready",
            request_id,
        )
    }

    pub(crate) fn internal(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "An internal error occurred",
            request_id,
        )
    }

    pub(crate) fn bad_request(
        request_id: RequestId,
        code: &'static str,
        message: &'static str,
    ) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message, request_id)
    }

    pub(crate) fn new(
        status: StatusCode,
        code: &'static str,
        message: impl Into<String>,
        request_id: RequestId,
    ) -> Self {
        Self {
            status,
            code,
            message: message.into(),
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
                    request_id: self.request_id.to_string(),
                },
            }),
        )
            .into_response()
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct ErrorEnvelope {
    pub(crate) error: ErrorBody,
}

#[derive(Debug, Serialize)]
pub(crate) struct ErrorBody {
    pub(crate) code: &'static str,
    pub(crate) message: String,
    pub(crate) request_id: String,
}

pub(crate) fn service_to_api_error(error: ServiceError, request_id: RequestId) -> ApiError {
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
        ServiceError::ObjectNotFound => ApiError::new(
            StatusCode::NOT_FOUND,
            "OBJECT_NOT_FOUND",
            "Object was not found",
            request_id,
        ),
        // A delete marker is a real version that deliberately hides the object.
        // Reporting it distinctly from a missing key tells the caller history
        // exists and can be restored.
        ServiceError::DeleteMarker(_) => ApiError::new(
            StatusCode::NOT_FOUND,
            "OBJECT_DELETED",
            "The object's current version is a delete marker",
            request_id,
        ),
        ServiceError::MultipartUploadNotFound => ApiError::new(
            StatusCode::NOT_FOUND,
            "MULTIPART_UPLOAD_NOT_FOUND",
            "Multipart upload was not found",
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

pub(crate) fn internal_service_error(error_value: ServiceError, request_id: RequestId) -> ApiError {
    error!(request_id = %request_id, error = %error_value, "management operation failed");
    ApiError::internal(request_id)
}

#[derive(Debug, Error)]
pub(crate) enum ReadinessError {
    #[error("storage dependency failed: {0}")]
    Storage(record_store_storage::StorageError),
    #[error("metadata dependency failed: {0}")]
    Metadata(record_store_metadata::MetadataError),
    #[error("audit dependency failed: {0}")]
    Audit(record_store_audit::AuditError),
    /// The capability store is not reachable.
    #[error("sharing dependency failed: {0}")]
    Sharing(String),
    #[error("event dependency failed: {0}")]
    Events(record_store_events::EventError),
    #[error("cluster dependency failed: {0}")]
    Cluster(String),
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
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use http_body_util::BodyExt;

    use super::*;
    use crate::dto::RequestId;

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
