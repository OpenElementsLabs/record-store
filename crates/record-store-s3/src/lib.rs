//! S3-compatible HTTP protocol adapter.
//!
//! Supported operations are deliberately limited to bucket lifecycle,
//! single-part object lifecycle, range GET, and ListObjectsV2.

use std::sync::Arc;

use axum::{
    Router,
    http::header::HeaderName,
    middleware::{self},
    routing::{get, put},
};
use chrono::Duration;
use record_store_audit::AuditRepository;
use record_store_auth::{Authorizer, SigningCredentialProvider};
use record_store_service::Services;

mod auth;
mod capabilities;
mod cors;
mod error;
mod handlers;
mod response;
mod sigv4;
mod xml;

#[cfg(test)]
mod tests;

pub use capabilities::{CapabilityStatus, S3_CAPABILITIES, S3Capability};

use auth::authenticate_request;
use cors::cors_preflight;
use handlers::bucket::{create_bucket, delete_bucket, head_bucket, list_buckets};
use handlers::listing::list_objects_v2;
use handlers::object::{delete_object, get_object, head_object, post_object, put_object};
use response::unsupported_operation;

pub(crate) const XML_CONTENT_TYPE: &str = "application/xml";
/// SHA-256 digest of an empty S3 request payload.
pub const EMPTY_PAYLOAD_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
pub(crate) const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-amz-request-id");

/// Dependencies and security settings for the S3 protocol surface.
#[derive(Clone)]
pub struct S3State {
    pub(crate) services: Services,
    pub(crate) credentials: Arc<dyn SigningCredentialProvider>,
    pub(crate) authorizer: Option<Arc<dyn Authorizer>>,
    pub(crate) audit: Option<Arc<dyn AuditRepository>>,
    pub(crate) root_s3_enabled: bool,
    pub(crate) allowed_clock_skew: Duration,
    pub(crate) maximum_presign_seconds: i64,
    pub(crate) maximum_header_bytes: usize,
}

impl S3State {
    /// Constructs S3 state with a 15-minute signed-request skew allowance.
    #[must_use]
    pub fn new(services: Services, credentials: Arc<dyn SigningCredentialProvider>) -> Self {
        Self {
            services,
            credentials,
            authorizer: None,
            audit: None,
            root_s3_enabled: true,
            allowed_clock_skew: Duration::minutes(15),
            maximum_presign_seconds: 604_800,
            maximum_header_bytes: 64 * 1024,
        }
    }

    /// Enables durable S3 security auditing.
    #[must_use]
    pub fn with_audit(mut self, audit: Arc<dyn AuditRepository>) -> Self {
        self.audit = Some(audit);
        self
    }

    /// Enables centralized policy evaluation for non-root principals.
    #[must_use]
    pub fn with_authorizer(mut self, authorizer: Arc<dyn Authorizer>) -> Self {
        self.authorizer = Some(authorizer);
        self
    }

    /// Controls whether the root signing credential may access the S3 surface.
    #[must_use]
    pub const fn with_root_s3_enabled(mut self, enabled: bool) -> Self {
        self.root_s3_enabled = enabled;
        self
    }

    /// Applies the maximum lifetime accepted for presigned URLs.
    #[must_use]
    pub const fn with_maximum_presign_seconds(mut self, seconds: i64) -> Self {
        self.maximum_presign_seconds = seconds;
        self
    }

    /// Applies the resolved aggregate S3 header limit.
    #[must_use]
    pub const fn with_maximum_header_bytes(mut self, maximum_header_bytes: usize) -> Self {
        self.maximum_header_bytes = maximum_header_bytes;
        self
    }
}

/// Builds the supported S3 API router.
pub fn router(state: S3State) -> Router {
    Router::new()
        .route("/", get(list_buckets))
        .route(
            "/{bucket}",
            put(create_bucket)
                .head(head_bucket)
                .delete(delete_bucket)
                .get(list_objects_v2)
                .options(cors_preflight),
        )
        .route(
            "/{bucket}/",
            put(create_bucket)
                .head(head_bucket)
                .delete(delete_bucket)
                .get(list_objects_v2)
                .options(cors_preflight),
        )
        .route(
            "/{bucket}/{*key}",
            put(put_object)
                .post(post_object)
                .get(get_object)
                .head(head_object)
                .delete(delete_object)
                .options(cors_preflight),
        )
        .fallback(unsupported_operation)
        .method_not_allowed_fallback(unsupported_operation)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            authenticate_request,
        ))
        .with_state(state)
}
