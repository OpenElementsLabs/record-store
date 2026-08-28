//! Shared translation of credential-store failures into API errors.

use axum::http::StatusCode;
use record_store_auth::CredentialStoreError;
use tracing::error;

use crate::dto::RequestId;
use crate::error::ApiError;

/// Maps a credential-store failure onto the status a client can act on.
///
/// Collapsing everything into `500` would tell a caller the server is broken
/// when it merely asked for an account that does not exist, and would bury the
/// genuine faults in the same log line as ordinary misses.
pub(crate) fn credential_error(error: CredentialStoreError, request_id: RequestId) -> ApiError {
    match error {
        CredentialStoreError::AccountNotFound
        | CredentialStoreError::CredentialNotFound
        | CredentialStoreError::PolicyNotFound => ApiError::not_found(request_id),
        CredentialStoreError::PolicyAlreadyExists => ApiError::new(
            StatusCode::CONFLICT,
            "POLICY_ALREADY_EXISTS",
            "A policy with that name already exists",
            request_id,
        ),
        CredentialStoreError::InvalidInput(reason) => {
            ApiError::new(StatusCode::BAD_REQUEST, "INVALID_INPUT", reason, request_id)
        }
        error => {
            error!(%error, request_id = %request_id, "credential operation failed");
            ApiError::internal(request_id)
        }
    }
}
