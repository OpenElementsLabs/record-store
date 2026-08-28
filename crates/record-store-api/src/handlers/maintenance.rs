use axum::{
    Json,
    extract::{Extension, Path, State},
    http::StatusCode,
};
use record_store_core::{BucketName, ObjectKey, ObjectMetadata, VersionId};
use record_store_service::ServiceListRequest;
use serde::{Deserialize, Serialize};

use crate::dto::ObjectSummary;
use crate::error::{ApiError, service_to_api_error};
use crate::handlers::objects::{parse_bucket_name, parse_object_key};
use crate::*;

#[derive(Deserialize)]
pub(crate) struct RestoreVersionRequest {
    version_id: VersionId,
}

pub(crate) async fn restore_version(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<RestoreVersionRequest>,
) -> Result<(StatusCode, Json<ObjectSummary>), ApiError> {
    let bucket = parse_bucket_name(&bucket, &request_id)?;
    let key = parse_object_key(&key, &request_id)?;
    state
        .services
        .objects
        .restore_version(&bucket, key, input.version_id)
        .await
        .map(|result| {
            (
                StatusCode::CREATED,
                Json(ObjectSummary::from(result.metadata)),
            )
        })
        .map_err(|error| service_to_api_error(error, request_id))
}

#[derive(Debug, Deserialize)]
pub(crate) struct CopyObjectRequest {
    /// Bucket the bytes are read from.
    source_bucket: String,
    /// Key the bytes are read from.
    source_key: String,
    /// Optional historical version to copy instead of the current one.
    #[serde(default)]
    source_version_id: Option<VersionId>,
    /// Replacement media type. Supplying any replacement field replaces the
    /// source's metadata rather than carrying it across.
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    custom_metadata: Option<std::collections::BTreeMap<String, String>>,
}

/// Copies an object server side.
///
/// The path names the destination, because that is what the request creates.
/// Bytes are streamed by the service layer and never buffered here, so copying
/// a large object costs the API process nothing beyond the transfer itself.
pub(crate) async fn copy_object(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<CopyObjectRequest>,
) -> Result<(StatusCode, Json<ObjectSummary>), ApiError> {
    let destination_bucket = parse_bucket_name(&bucket, &request_id)?;
    let destination_key = parse_object_key(&key, &request_id)?;
    let source_bucket = parse_bucket_name(&input.source_bucket, &request_id)?;
    let source_key = parse_object_key(&input.source_key, &request_id)?;
    let replaces_metadata = input.content_type.is_some() || input.custom_metadata.is_some();
    state
        .services
        .objects
        .copy(record_store_service::ServiceCopyRequest {
            source_bucket,
            source_key,
            source_version_id: input.source_version_id,
            destination_bucket,
            destination_key,
            metadata_directive: if replaces_metadata {
                record_store_service::CopyMetadataDirective::Replace
            } else {
                record_store_service::CopyMetadataDirective::Copy
            },
            content_type: input.content_type,
            replacement_metadata: input.custom_metadata.unwrap_or_default(),
        })
        .await
        .map(|result| {
            (
                StatusCode::CREATED,
                Json(ObjectSummary::from(result.metadata)),
            )
        })
        .map_err(|error| service_to_api_error(error, request_id))
}

pub(crate) async fn verify_object(
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
pub(crate) struct VerifyBucketResponse {
    verified_objects: u64,
    failures: u64,
}

pub(crate) async fn verify_bucket(
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

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;

    use crate::test_support::{admin, api, call, expect_status, make_bucket, put_object};

    #[tokio::test]
    async fn verifying_a_stored_object_confirms_its_checksum() {
        let (_directory, api) = api().await;
        make_bucket(&api, "photos").await;
        put_object(&api, "photos", "a.txt", b"contents").await;

        let verified = expect_status(
            &api,
            admin("POST", "/api/v1/verify/objects/photos/a.txt", None),
            StatusCode::OK,
        )
        .await;
        assert_eq!(verified["size"], 8, "{verified}");
    }

    #[tokio::test]
    async fn verifying_a_whole_bucket_reports_every_object_it_checked() {
        let (_directory, api) = api().await;
        make_bucket(&api, "photos").await;
        put_object(&api, "photos", "a.txt", b"one").await;
        put_object(&api, "photos", "b.txt", b"two").await;

        let report = expect_status(
            &api,
            admin("POST", "/api/v1/verify/buckets/photos", None),
            StatusCode::OK,
        )
        .await;
        assert_eq!(report["verified_objects"], 2, "{report}");
        assert_eq!(report["failures"], 0, "{report}");
    }

    #[tokio::test]
    async fn verifying_an_object_that_does_not_exist_is_a_not_found() {
        let (_directory, api) = api().await;
        make_bucket(&api, "photos").await;
        expect_status(
            &api,
            admin("POST", "/api/v1/verify/objects/photos/absent.txt", None),
            StatusCode::NOT_FOUND,
        )
        .await;
    }

    /// A copy reads one object and writes another; the destination must hold the
    /// source's bytes without the caller ever streaming them.
    #[tokio::test]
    async fn a_server_side_copy_reproduces_the_source_object() {
        let (_directory, api) = api().await;
        make_bucket(&api, "photos").await;
        put_object(&api, "photos", "original.txt", b"source-bytes").await;

        let copied = expect_status(
            &api,
            admin(
                "POST",
                "/api/v1/buckets/photos/object-copy/duplicate.txt",
                Some(json!({"source_bucket": "photos", "source_key": "original.txt"})),
            ),
            StatusCode::CREATED,
        )
        .await;
        assert_eq!(copied["size"], 12, "{copied}");
        assert_eq!(copied["key"], "duplicate.txt", "{copied}");
    }

    /// Supplying replacement metadata replaces the source's rather than merging,
    /// which is the documented behaviour and the one an operator relies on when
    /// correcting a wrong media type.
    #[tokio::test]
    async fn a_copy_can_replace_the_source_metadata() {
        let (_directory, api) = api().await;
        make_bucket(&api, "photos").await;
        put_object(&api, "photos", "original.bin", b"bytes").await;

        let copied = expect_status(
            &api,
            admin(
                "POST",
                "/api/v1/buckets/photos/object-copy/typed.bin",
                Some(json!({
                    "source_bucket": "photos",
                    "source_key": "original.bin",
                    "content_type": "text/plain",
                })),
            ),
            StatusCode::CREATED,
        )
        .await;
        assert_eq!(copied["content_type"], "text/plain", "{copied}");
    }

    #[tokio::test]
    async fn copying_from_a_source_that_does_not_exist_is_a_not_found() {
        let (_directory, api) = api().await;
        make_bucket(&api, "photos").await;
        expect_status(
            &api,
            admin(
                "POST",
                "/api/v1/buckets/photos/object-copy/x.txt",
                Some(json!({"source_bucket": "photos", "source_key": "absent.txt"})),
            ),
            StatusCode::NOT_FOUND,
        )
        .await;
    }

    /// Restoring makes an older version current again without destroying the
    /// newer one, which is the whole point of keeping versions.
    #[tokio::test]
    async fn an_older_version_can_be_restored_as_the_current_one() {
        let (_directory, api) = api().await;
        make_bucket(&api, "photos").await;
        expect_status(
            &api,
            admin(
                "PUT",
                "/api/v1/buckets/photos/versioning",
                Some(json!({"versioning": "enabled"})),
            ),
            StatusCode::OK,
        )
        .await;

        let first = put_object(&api, "photos", "note.txt", b"first").await;
        put_object(&api, "photos", "note.txt", b"second-longer").await;
        let original = first["version_id"].as_str().expect("version id").to_owned();

        let restored = expect_status(
            &api,
            admin(
                "POST",
                "/api/v1/restore/photos/note.txt",
                Some(json!({"version_id": original})),
            ),
            StatusCode::CREATED,
        )
        .await;
        assert_eq!(
            restored["size"], 5,
            "the restored copy must carry the older version's bytes: {restored}"
        );
    }

    #[tokio::test]
    async fn restoring_a_version_that_does_not_exist_is_refused() {
        let (_directory, api) = api().await;
        make_bucket(&api, "photos").await;
        put_object(&api, "photos", "note.txt", b"only").await;

        let response = call(
            &api,
            admin(
                "POST",
                "/api/v1/restore/photos/note.txt",
                Some(json!({"version_id": "0195f0c8-0000-7000-8000-0000000000ff"})),
            ),
        )
        .await;
        assert!(
            response.status().is_client_error(),
            "restoring an unknown version must not succeed: {}",
            response.status()
        );
    }
}
