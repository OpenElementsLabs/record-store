use axum::{
    Json,
    extract::{Extension, Path, Query, Request, State},
    http::{HeaderValue, StatusCode, header, header::HeaderName},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use record_store_core::{BucketName, ObjectKey, ObjectMetadata, PreviewKind, VersionId};
use record_store_service::ServiceListRequest;
use tracing::error;

use crate::dto::{
    DeleteVersionQuery, EventQueryParameters, EventsResponse, ObjectListQuery, ObjectListResponse,
    ObjectSummary, ObjectVersionEntry, ObjectVersionListQuery, ObjectVersionListResponse,
    PreviewQuery, parse_preview_range,
};
use crate::error::{ApiError, service_to_api_error};
use crate::handlers::webhooks::event_repository;
use crate::*;

/// Lists objects under a prefix, one bounded page at a time.
///
/// Listing is always paginated: a bucket may hold millions of objects, so no
/// caller is ever handed the whole keyspace.
pub(crate) async fn list_bucket_objects(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    Query(query): Query<ObjectListQuery>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<ObjectListResponse>, ApiError> {
    let name = parse_bucket_name(&bucket, &request_id)?;
    if query.limit == 0 || query.limit > 1_000 {
        return Err(ApiError::bad_request(
            request_id,
            "INVALID_LIMIT",
            "limit must be between 1 and 1000",
        ));
    }
    let start_after = match &query.continuation_token {
        Some(token) => Some(decode_cursor(token, &request_id)?),
        None => None,
    };
    let result = state
        .services
        .objects
        .list(ServiceListRequest {
            bucket: name,
            prefix: query.prefix,
            delimiter: query.delimiter,
            maximum_keys: query.limit,
            start_after,
        })
        .await
        .map_err(|error| service_to_api_error(error, request_id))?;
    Ok(Json(ObjectListResponse {
        objects: result
            .objects
            .into_iter()
            .map(ObjectSummary::from)
            .collect(),
        prefixes: result.common_prefixes.into_iter().collect(),
        is_truncated: result.is_truncated,
        next_continuation_token: result.next_marker.as_deref().map(encode_cursor),
    }))
}

/// Returns one object's metadata without transferring its bytes.
///
/// A `version_id` names an exact immutable version. Without one the current
/// version is returned; the two are never substituted for one another, because a
/// caller inspecting history that silently receives the current metadata has
/// been told something false.
pub(crate) async fn get_bucket_object(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    Query(query): Query<PreviewQuery>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<ObjectSummary>, ApiError> {
    let name = parse_bucket_name(&bucket, &request_id)?;
    let key = parse_object_key(&key, &request_id)?;
    match query.version_id {
        Some(version_id) => {
            state
                .services
                .objects
                .head_version(&name, key, version_id)
                .await
        }
        None => state.services.objects.head(&name, key).await,
    }
    .map(|metadata| Json(ObjectSummary::from(metadata)))
    .map_err(|error| service_to_api_error(error, request_id))
}

/// Streams an object's bytes to the caller.
///
/// The payload is streamed rather than buffered, so object size is bounded by
/// storage rather than by this process's memory.
pub(crate) async fn download_bucket_object(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    Query(query): Query<PreviewQuery>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Response, ApiError> {
    let name = parse_bucket_name(&bucket, &request_id)?;
    let key = parse_object_key(&key, &request_id)?;
    let result = match query.version_id {
        Some(version_id) => {
            state
                .services
                .objects
                .get_version(&name, key.clone(), version_id, None)
                .await
        }
        None => state.services.objects.get(&name, key.clone(), None).await,
    }
    .map_err(|error| service_to_api_error(error, request_id.clone()))?;
    // A download is an attachment whatever the bytes turn out to be, so the
    // declared media type is carried through rather than reinterpreted. What
    // makes that safe is the disposition and `nosniff` below, not the type.
    let content_type = result
        .metadata
        .content_type
        .clone()
        .unwrap_or_else(|| "application/octet-stream".to_owned());
    let filename = key
        .as_str()
        .rsplit('/')
        .next()
        .unwrap_or("download")
        .to_owned();
    let mut response = Response::new(axum::body::Body::from_stream(result.body));
    let headers = response.headers_mut();
    insert_header(headers, header::CONTENT_TYPE, &content_type);
    insert_header(
        headers,
        header::CONTENT_LENGTH,
        &result.metadata.size.to_string(),
    );
    insert_header(
        headers,
        header::ETAG,
        &format!("\"{}\"", result.metadata.etag.as_str()),
    );
    // The filename is quoted and escaped so a key containing quotes cannot
    // break out of the header value.
    insert_header(
        headers,
        header::CONTENT_DISPOSITION,
        &format!(
            "attachment; filename=\"{}\"",
            filename.replace(['\\', '"'], "_")
        ),
    );
    // The declared type is caller-supplied, so the browser is told plainly not
    // to look for a better one.
    insert_header(headers, header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    insert_header(
        headers,
        header::CACHE_CONTROL,
        "private, no-store, max-age=0",
    );
    Ok(response)
}

/// Streams an explicitly safe inline preview without changing download semantics.
///
/// Preview and download are two different promises. Download hands an operator
/// an attachment whatever the bytes turn out to be; preview asks a browser to
/// interpret them, and so it is only ever offered for media types Record Store is willing
/// to be responsible for. This route therefore refuses far more than the
/// download route does, and its refusals are the point rather than a gap.
pub(crate) async fn preview_bucket_object(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    Query(query): Query<PreviewQuery>,
    headers: header::HeaderMap,
    Extension(request_id): Extension<RequestId>,
) -> Result<Response, ApiError> {
    state.sharing_metrics.preview_request();
    let name = parse_bucket_name(&bucket, &request_id)?;
    let object_key = parse_object_key(&key, &request_id)?;
    // The version the caller asked for is the version that is inspected, and
    // later the version that is read. Falling back to "current" when a specific
    // one was requested would quietly show the wrong bytes.
    let metadata = match query.version_id {
        Some(version_id) => {
            state
                .services
                .objects
                .head_version(&name, object_key.clone(), version_id)
                .await
        }
        None => state.services.objects.head(&name, object_key.clone()).await,
    }
    .inspect_err(|_| state.sharing_metrics.preview_failure())
    .map_err(|error| service_to_api_error(error, request_id.clone()))?;

    let kind = PreviewKind::classify(metadata.content_type.as_deref());
    let Some(content_type) =
        PreviewKind::canonical_content_type(metadata.content_type.as_deref().unwrap_or_default())
            .filter(|_| kind.allows_inline())
    else {
        state.sharing_metrics.preview_failure();
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "PREVIEW_UNSUPPORTED",
            "This object type cannot be previewed safely",
            request_id,
        ));
    };
    // The stored media type came from whoever uploaded the object. Serving it
    // inline on that word alone would let an uploader choose how a browser
    // interprets their bytes, so the object's own leading bytes have to agree.
    verify_preview_signature(
        &state,
        &name,
        &object_key,
        query.version_id,
        &metadata,
        &request_id,
    )
    .await
    .inspect_err(|_| state.sharing_metrics.preview_failure())?;

    let range = parse_preview_range(&headers, metadata.size, &request_id)?;
    let result = match query.version_id {
        Some(version_id) => {
            state
                .services
                .objects
                .get_version(&name, object_key, version_id, range)
                .await
        }
        None => state.services.objects.get(&name, object_key, range).await,
    }
    .inspect_err(|_| state.sharing_metrics.preview_failure())
    .map_err(|error| service_to_api_error(error, request_id.clone()))?;
    let length = result
        .range
        .map_or(result.metadata.size, |value| value.length);
    let status = if result.range.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let mut response = (
        status,
        axum::body::Body::from_stream(futures_util::TryStreamExt::map_err(
            result.body,
            std::io::Error::other,
        )),
    )
        .into_response();
    let response_headers = response.headers_mut();
    insert_header(response_headers, header::CONTENT_TYPE, content_type);
    insert_header(
        response_headers,
        header::CONTENT_LENGTH,
        &length.to_string(),
    );
    insert_header(response_headers, header::ACCEPT_RANGES, "bytes");
    insert_header(response_headers, header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    insert_header(response_headers, header::CONTENT_DISPOSITION, "inline");
    insert_header(
        response_headers,
        header::ETAG,
        &format!("\"{}\"", result.metadata.etag.as_str()),
    );
    // Preview bytes are authenticated content that must not outlive the session
    // that fetched them in any shared cache.
    insert_header(
        response_headers,
        header::CACHE_CONTROL,
        "private, no-store, max-age=0",
    );
    // Stored bytes get their own opaque origin. A PDF still renders, and
    // anything it tries to execute has no origin to execute against.
    insert_header(
        response_headers,
        header::CONTENT_SECURITY_POLICY,
        sharing::SHARE_CONTENT_POLICY,
    );
    if let Some(range) = result.range {
        insert_header(
            response_headers,
            header::CONTENT_RANGE,
            &format!(
                "bytes {}-{}/{}",
                range.offset,
                range.offset + range.length - 1,
                result.metadata.size
            ),
        );
    }
    Ok(response)
}

/// Confirms an object's leading bytes agree with the media type it claims.
pub(crate) async fn verify_preview_signature(
    state: &AppState,
    bucket: &BucketName,
    key: &ObjectKey,
    version_id: Option<VersionId>,
    metadata: &ObjectMetadata,
    request_id: &RequestId,
) -> Result<(), ApiError> {
    let Some(content_type) = metadata.content_type.as_deref() else {
        return Ok(());
    };
    if metadata.size == 0 {
        return Ok(());
    }
    let probe_length = metadata
        .size
        .min(record_store_core::CONTENT_SIGNATURE_PROBE_BYTES as u64);
    let Ok(range) = record_store_core::ByteRange::new(0, probe_length) else {
        return Ok(());
    };
    let result = match version_id {
        Some(version_id) => {
            state
                .services
                .objects
                .get_version(bucket, key.clone(), version_id, Some(range))
                .await
        }
        None => {
            state
                .services
                .objects
                .get(bucket, key.clone(), Some(range))
                .await
        }
    }
    .map_err(|error| service_to_api_error(error, request_id.clone()))?;
    let prefix = sharing::read_probe(result).await.map_err(|error| {
        error!(%error, request_id = %request_id, "content signature probe failed");
        ApiError::internal(request_id.clone())
    })?;
    if record_store_core::content_signature_matches(content_type, &prefix) {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "CONTENT_TYPE_MISMATCH",
            "This object's contents do not match its recorded media type, so it will not be shown inline",
            request_id.clone(),
        ))
    }
}

/// Streams an uploaded object into storage.
pub(crate) async fn upload_bucket_object(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
    request: Request,
) -> Result<(StatusCode, Json<ObjectSummary>), ApiError> {
    let name = parse_bucket_name(&bucket, &request_id)?;
    let object_key = parse_object_key(&key, &request_id)?;
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = request.into_body().into_data_stream();
    let stream = futures_util::TryStreamExt::map_err(body, std::io::Error::other);
    let result = state
        .services
        .objects
        .put(record_store_service::ServicePutRequest {
            bucket: name,
            key: object_key,
            content_type,
            custom_metadata: std::collections::BTreeMap::new(),
            expected_checksum: None,
            body: record_store_storage::upload_stream(stream),
        })
        .await
        .map_err(|error| service_to_api_error(error, request_id))?;
    Ok((
        StatusCode::CREATED,
        Json(ObjectSummary::from(result.metadata)),
    ))
}

/// Deletes the visible version of an object.
pub(crate) async fn delete_bucket_object(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
) -> Result<StatusCode, ApiError> {
    let name = parse_bucket_name(&bucket, &request_id)?;
    let key = parse_object_key(&key, &request_id)?;
    let removed = state
        .services
        .objects
        .delete(&name, key)
        .await
        .map_err(|error| service_to_api_error(error, request_id.clone()))?;
    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "OBJECT_NOT_FOUND",
            "Object was not found",
            request_id,
        ))
    }
}

/// Lists the version history under a prefix.
pub(crate) async fn list_bucket_object_versions(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    Query(query): Query<ObjectVersionListQuery>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<ObjectVersionListResponse>, ApiError> {
    let name = parse_bucket_name(&bucket, &request_id)?;
    if query.limit == 0 || query.limit > 1_000 {
        return Err(ApiError::bad_request(
            request_id,
            "INVALID_LIMIT",
            "limit must be between 1 and 1000",
        ));
    }
    if query.key_marker.is_some() != query.version_id_marker.is_some() {
        return Err(ApiError::bad_request(
            request_id,
            "INVALID_VERSION_CURSOR",
            "Both version cursor fields are required",
        ));
    }
    let result = state
        .services
        .objects
        .list_versions(record_store_service::ServiceListVersionsRequest {
            bucket: name,
            prefix: query.prefix,
            key_marker: query.key_marker,
            version_id_marker: query.version_id_marker,
            maximum_keys: query.limit,
        })
        .await
        .map_err(|error| service_to_api_error(error, request_id))?;
    Ok(Json(ObjectVersionListResponse {
        versions: result
            .versions
            .into_iter()
            .map(|listed| {
                let is_latest = listed.is_latest;
                match listed.record {
                    record_store_core::ObjectVersionRecord::Object { metadata, is_null } => {
                        ObjectVersionEntry {
                            key: metadata.key.to_string(),
                            version_id: metadata.version_id,
                            is_latest,
                            is_delete_marker: false,
                            is_null,
                            created_at: metadata.created_at,
                            size: Some(metadata.size),
                            etag: Some(metadata.etag.as_str().to_owned()),
                            checksum: Some(metadata.checksum.to_string()),
                        }
                    }
                    record_store_core::ObjectVersionRecord::DeleteMarker { marker, is_null } => {
                        ObjectVersionEntry {
                            key: marker.key.to_string(),
                            version_id: marker.version_id,
                            is_latest,
                            is_delete_marker: true,
                            is_null,
                            created_at: marker.created_at,
                            size: None,
                            etag: None,
                            checksum: None,
                        }
                    }
                }
            })
            .collect(),
        next_key_marker: result.next_key_marker,
        next_version_id_marker: result.next_version_id_marker,
    }))
}

/// Permanently removes one object version.
pub(crate) async fn delete_bucket_object_version(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    Query(query): Query<DeleteVersionQuery>,
    Extension(request_id): Extension<RequestId>,
) -> Result<StatusCode, ApiError> {
    let name = parse_bucket_name(&bucket, &request_id)?;
    let key = parse_object_key(&key, &request_id)?;
    state
        .services
        .objects
        .delete_version(&name, key, query.version_id)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|error| service_to_api_error(error, request_id))
}

/// Returns recent storage events, newest first.
///
/// Storage events record what happened to data. They are intentionally a
/// different feed from the audit trail, which records who requested it.
pub(crate) async fn list_storage_events(
    State(state): State<AppState>,
    Query(query): Query<EventQueryParameters>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<EventsResponse>, ApiError> {
    let events = event_repository(&state, &request_id)?;
    let after = match (query.after_time, query.after_id) {
        (Some(time), Some(id)) => Some((time, id)),
        (None, None) => None,
        _ => {
            return Err(ApiError::bad_request(
                request_id,
                "INVALID_EVENT_CURSOR",
                "Both event cursor fields are required",
            ));
        }
    };
    let page = events
        .list_events(record_store_events::EventQuery {
            since: query.since,
            until: query.until,
            bucket: query.bucket,
            event_type: query.event_type,
            object_prefix: query.prefix,
            after,
            limit: query.limit,
        })
        .await
        .map_err(|error| {
            error!(%error, request_id = %request_id, "storage event query failed");
            ApiError::internal(request_id)
        })?;
    let (next_time, next_id) = page
        .next
        .map_or((None, None), |(time, id)| (Some(time), Some(id)));
    Ok(Json(EventsResponse {
        events: page.events,
        next_time,
        next_id,
    }))
}

pub(crate) fn insert_header(headers: &mut header::HeaderMap, name: HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

pub(crate) fn parse_bucket_name(
    value: &str,
    request_id: &RequestId,
) -> Result<BucketName, ApiError> {
    BucketName::new(value).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_BUCKET_NAME",
            "Invalid bucket name",
        )
    })
}

pub(crate) fn parse_object_key(value: &str, request_id: &RequestId) -> Result<ObjectKey, ApiError> {
    ObjectKey::new(value).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_OBJECT_KEY",
            "Invalid object key",
        )
    })
}

/// Pagination cursors are opaque so clients cannot build on their internals.
pub(crate) fn encode_cursor(value: &str) -> String {
    URL_SAFE_NO_PAD.encode(value.as_bytes())
}

pub(crate) fn decode_cursor(value: &str, request_id: &RequestId) -> Result<String, ApiError> {
    let invalid = || {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_CONTINUATION_TOKEN",
            "Invalid continuation token",
        )
    };
    let decoded = URL_SAFE_NO_PAD.decode(value).map_err(|_| invalid())?;
    String::from_utf8(decoded).map_err(|_| invalid())
}
