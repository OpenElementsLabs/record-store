use std::io;

use axum::{
    body::Body,
    extract::{Extension, Path, RawQuery, State},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{self, HeaderName},
    },
    response::{IntoResponse, Response},
};
use futures_util::TryStreamExt;
use percent_encoding::percent_decode_str;
use record_store_auth::Principal;
use record_store_core::{
    Checksum, CompletedPart, ETag, ObjectKey as CoreObjectKey, PartNumber, UploadId, VersionId,
};
use record_store_service::{
    CopyMetadataDirective, LockContext, ServiceCompleteMultipartRequest, ServiceCopyRequest,
    ServiceCreateMultipartRequest, ServicePutRequest, ServiceUploadPartRequest,
};
use record_store_storage::upload_stream;
use sha2::{Digest, Sha256};

use crate::auth::principal_name;
use crate::checksum::{Mismatch, RequestDigest, read_verified, request_digests, verify_streamed};
use crate::error::{S3Error, S3ErrorKind, service_error};
use crate::handlers::listing::{RequestedVersion, list_parts, query_map, requested_version};
use crate::response::{
    ConditionalOutcome, apply_object_headers, apply_object_lock_headers, bucket_name,
    conditional_streaming_response, custom_metadata, evaluate_conditions, insert_etag,
    insert_version_id, object_key, parse_range, reject_subresources, requested_governance_bypass,
    requested_object_lock, unsupported_put_headers, xml_response,
};
use crate::sigv4::{PayloadHash, S3RequestId, request_checksum};
use crate::xml::CompleteMultipartUploadDocument;
use crate::xml::{
    CompleteMultipartUploadResult, CopyObjectResult, InitiateMultipartUploadResult,
    LegalHoldDocument, LegalHoldResult, RetentionDocument, RetentionResult, format_retain_until,
    legal_hold_status, parse_legal_hold,
};
use crate::*;

#[expect(
    clippy::too_many_arguments,
    reason = "an axum handler's inputs are its extractors; bundling them behind \
              another type would hide the request surface rather than shrink it"
)]
pub(crate) async fn put_object(
    State(state): State<S3State>,
    Path((bucket, key)): Path<(String, String)>,
    RawQuery(raw_query): RawQuery,
    Extension(request_id): Extension<S3RequestId>,
    Extension(payload_hash): Extension<PayloadHash>,
    principal: Option<Extension<Principal>>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, S3Error> {
    let query = query_map(raw_query.as_deref())
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &format!("/{bucket}/{key}")))?;
    if query.contains_key("retention") {
        return put_object_retention(
            state, bucket, key, &query, request_id, principal, &headers, body,
        )
        .await;
    }
    if query.contains_key("legal-hold") {
        return put_object_legal_hold(
            state, bucket, key, &query, request_id, principal, &headers, body,
        )
        .await;
    }
    let expected_checksum = request_checksum(&headers, &payload_hash)
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &format!("/{bucket}/{key}")))?;
    let digests = request_digests(&headers)
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &format!("/{bucket}/{key}")))?;
    let echoes: Vec<_> = digests.iter().filter_map(RequestDigest::echo).collect();
    if let (Some(upload_id), Some(part_number)) = (query.get("uploadId"), query.get("partNumber")) {
        let bucket_name = bucket_name(&bucket, &request_id)?;
        let object_key = object_key(&key, &request_id, &format!("/{bucket}/{key}"))?;
        let upload_id = upload_id.parse::<UploadId>().map_err(|_| {
            S3Error::new(
                S3ErrorKind::NoSuchUpload,
                request_id.clone(),
                &format!("/{bucket}/{key}"),
            )
        })?;
        let number = part_number.parse::<PartNumber>().map_err(|_| {
            S3Error::new(
                S3ErrorKind::InvalidRequest,
                request_id.clone(),
                &format!("/{bucket}/{key}"),
            )
        })?;
        if headers.contains_key("x-amz-copy-source") {
            return Err(S3Error::new(
                S3ErrorKind::NotImplemented,
                request_id,
                &format!("/{bucket}/{key}"),
            ));
        }
        let (body, mismatch) = verified_upload(body, digests);
        let part = state
            .services
            .objects
            .upload_part(ServiceUploadPartRequest {
                bucket: bucket_name,
                key: object_key,
                upload_id,
                number,
                expected_checksum: expected_checksum.clone(),
                body,
            })
            .await
            .map_err(|error| {
                upload_error(error, &mismatch, &request_id, &format!("/{bucket}/{key}"))
            })?;
        let mut response = StatusCode::OK.into_response();
        insert_echoes(&mut response, &echoes);
        if let Ok(value) = HeaderValue::from_str(&format!("\"{}\"", part.etag)) {
            response.headers_mut().insert(header::ETAG, value);
        }
        return Ok(response);
    }
    if headers.contains_key("x-amz-copy-source") {
        return copy_object(state, bucket, key, request_id, headers).await;
    }
    reject_subresources(
        raw_query.as_deref(),
        &request_id,
        &format!("/{bucket}/{key}"),
    )?;
    if unsupported_put_headers(&headers) {
        return Err(S3Error::new(
            S3ErrorKind::NotImplemented,
            request_id,
            &format!("/{bucket}/{key}"),
        ));
    }
    let bucket_name = bucket_name(&bucket, &request_id)?;
    let object_key = object_key(&key, &request_id, &format!("/{bucket}/{key}"))?;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let custom_metadata = custom_metadata(&headers, &request_id, &format!("/{bucket}/{key}"))?;
    let object_lock = requested_object_lock(&headers, &request_id, &format!("/{bucket}/{key}"))?;
    let (body, mismatch) = verified_upload(body, digests);
    let result = state
        .services
        .objects
        .put(ServicePutRequest {
            bucket: bucket_name,
            key: object_key,
            content_type,
            custom_metadata,
            expected_checksum,
            object_lock,
            body,
        })
        .await
        .map_err(|error| {
            upload_error(error, &mismatch, &request_id, &format!("/{bucket}/{key}"))
        })?;
    let mut response = StatusCode::OK.into_response();
    insert_echoes(&mut response, &echoes);
    insert_etag(&mut response, &result.metadata);
    insert_version_id(&mut response, result.metadata.version_id);
    Ok(response)
}

/// An upload body verified while it streams against the digests the request
/// carries (`Content-MD5`, `x-amz-checksum-*`; see `crate::checksum`).
fn verified_upload(
    body: Body,
    digests: Vec<RequestDigest>,
) -> (record_store_storage::UploadStream, Mismatch) {
    let (stream, mismatch) =
        verify_streamed(body.into_data_stream().map_err(io::Error::other), digests);
    (upload_stream(stream), mismatch)
}

/// A failed upload whose body contradicted its own digest is a `BadDigest`,
/// however the storage layer happened to report the aborted write.
fn upload_error(
    error: record_store_service::ServiceError,
    mismatch: &Mismatch,
    request_id: &S3RequestId,
    resource: &str,
) -> S3Error {
    if mismatch.happened() {
        S3Error::new(S3ErrorKind::BadDigest, request_id.clone(), resource)
    } else {
        service_error(error, request_id.clone(), resource)
    }
}

fn insert_echoes(response: &mut Response, echoes: &[(&'static str, String)]) {
    for (name, value) in echoes {
        if let Ok(value) = HeaderValue::from_str(value) {
            response
                .headers_mut()
                .insert(HeaderName::from_static(name), value);
        }
    }
}

pub(crate) async fn get_object(
    State(state): State<S3State>,
    Path((bucket, key)): Path<(String, String)>,
    RawQuery(raw_query): RawQuery,
    Extension(request_id): Extension<S3RequestId>,
    headers: HeaderMap,
) -> Result<Response, S3Error> {
    let query = query_map(raw_query.as_deref())
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &format!("/{bucket}/{key}")))?;
    if let Some(upload_id) = query.get("uploadId") {
        return list_parts(state, bucket, key, upload_id, &query, request_id).await;
    }
    if query.contains_key("retention") {
        return get_object_retention(state, bucket, key, &query, request_id).await;
    }
    if query.contains_key("legal-hold") {
        return get_object_legal_hold(state, bucket, key, &query, request_id).await;
    }
    let version = requested_version(&query)
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &format!("/{bucket}/{key}")))?;
    reject_subresources(
        raw_query.as_deref(),
        &request_id,
        &format!("/{bucket}/{key}"),
    )?;
    let bucket_name = bucket_name(&bucket, &request_id)?;
    let object_key = object_key(&key, &request_id, &format!("/{bucket}/{key}"))?;
    let range = if let Some(value) = headers.get(header::RANGE) {
        let value = value
            .to_str()
            .map_err(|_| S3Error::new(S3ErrorKind::InvalidRange, request_id.clone(), &key))?;
        let metadata = match version {
            Some(RequestedVersion::Id(version_id)) => {
                state
                    .services
                    .objects
                    .head_version(&bucket_name, object_key.clone(), version_id)
                    .await
            }
            Some(RequestedVersion::Null) => {
                state
                    .services
                    .objects
                    .head_null_version(&bucket_name, object_key.clone())
                    .await
            }
            None => {
                state
                    .services
                    .objects
                    .head(&bucket_name, object_key.clone())
                    .await
            }
        }
        .map_err(|error| service_error(error, request_id.clone(), &key))?;
        Some(
            parse_range(value, metadata.size)
                .map_err(|kind| S3Error::new(kind, request_id.clone(), &key))?,
        )
    } else {
        None
    };
    let result = match version {
        Some(RequestedVersion::Id(version_id)) => {
            state
                .services
                .objects
                .get_version(&bucket_name, object_key, version_id, range)
                .await
        }
        Some(RequestedVersion::Null) => {
            state
                .services
                .objects
                .get_null_version(&bucket_name, object_key, range)
                .await
        }
        None => {
            state
                .services
                .objects
                .get(&bucket_name, object_key, range)
                .await
        }
    }
    .map_err(|error| service_error(error, request_id.clone(), &format!("/{bucket}/{key}")))?;
    let version_id = result.metadata.version_id;
    let mut response =
        conditional_streaming_response(result, &headers, request_id, &format!("/{bucket}/{key}"))?;
    if response.status() != StatusCode::NOT_MODIFIED {
        attach_object_lock_headers(&state, &mut response, version_id).await;
    }
    Ok(response)
}

pub(crate) async fn head_object(
    State(state): State<S3State>,
    Path((bucket, key)): Path<(String, String)>,
    RawQuery(raw_query): RawQuery,
    Extension(request_id): Extension<S3RequestId>,
    headers: HeaderMap,
) -> Result<Response, S3Error> {
    let query = query_map(raw_query.as_deref())
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &format!("/{bucket}/{key}")))?;
    let version =
        requested_version(&query).map_err(|kind| S3Error::new(kind, request_id.clone(), &key))?;
    reject_subresources(
        raw_query.as_deref(),
        &request_id,
        &format!("/{bucket}/{key}"),
    )?;
    let bucket_name = bucket_name(&bucket, &request_id)?;
    let object_key = object_key(&key, &request_id, &format!("/{bucket}/{key}"))?;
    let metadata = match version {
        Some(RequestedVersion::Id(version_id)) => {
            state
                .services
                .objects
                .head_version(&bucket_name, object_key, version_id)
                .await
        }
        Some(RequestedVersion::Null) => {
            state
                .services
                .objects
                .head_null_version(&bucket_name, object_key)
                .await
        }
        None => state.services.objects.head(&bucket_name, object_key).await,
    }
    .map_err(|error| service_error(error, request_id.clone(), &format!("/{bucket}/{key}")))?;
    if evaluate_conditions(&metadata, &headers)
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &format!("/{bucket}/{key}")))?
        == ConditionalOutcome::NotModified
    {
        return Ok(StatusCode::NOT_MODIFIED.into_response());
    }
    let mut response = StatusCode::OK.into_response();
    apply_object_headers(&mut response, &metadata, metadata.size);
    insert_version_id(&mut response, metadata.version_id);
    attach_object_lock_headers(&state, &mut response, metadata.version_id).await;
    Ok(response)
}

pub(crate) async fn delete_object(
    State(state): State<S3State>,
    Path((bucket, key)): Path<(String, String)>,
    RawQuery(raw_query): RawQuery,
    Extension(request_id): Extension<S3RequestId>,
    principal: Option<Extension<Principal>>,
    headers: HeaderMap,
) -> Result<Response, S3Error> {
    let query = query_map(raw_query.as_deref())
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &format!("/{bucket}/{key}")))?;
    if let Some(upload_id) = query.get("uploadId") {
        let bucket_name = bucket_name(&bucket, &request_id)?;
        let object_key = object_key(&key, &request_id, &key)?;
        let upload_id = upload_id
            .parse::<UploadId>()
            .map_err(|_| S3Error::new(S3ErrorKind::NoSuchUpload, request_id.clone(), &key))?;
        state
            .services
            .objects
            .abort_multipart(&bucket_name, &object_key, upload_id)
            .await
            .map_err(|error| service_error(error, request_id, &key))?;
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let version =
        requested_version(&query).map_err(|kind| S3Error::new(kind, request_id.clone(), &key))?;
    reject_subresources(
        raw_query.as_deref(),
        &request_id,
        &format!("/{bucket}/{key}"),
    )?;
    let bucket_name = bucket_name(&bucket, &request_id)?;
    let object_key = object_key(&key, &request_id, &format!("/{bucket}/{key}"))?;
    // Presenting the bypass header requires s3:BypassGovernanceRetention, which
    // the authorization middleware enforced before this handler ran, so its
    // presence here already means an authorized bypass.
    let context = lock_context(principal.as_ref(), &headers);
    let mut response = StatusCode::NO_CONTENT.into_response();
    match version {
        Some(RequestedVersion::Id(version_id)) => {
            state
                .services
                .objects
                .delete_version(&bucket_name, object_key, version_id, &context)
                .await
                .map_err(|error| service_error(error, request_id, &format!("/{bucket}/{key}")))?;
            insert_version_id(&mut response, version_id);
        }
        Some(RequestedVersion::Null) => {
            state
                .services
                .objects
                .delete_null_version(&bucket_name, object_key, &context)
                .await
                .map_err(|error| service_error(error, request_id, &format!("/{bucket}/{key}")))?;
            response.headers_mut().insert(
                HeaderName::from_static("x-amz-version-id"),
                HeaderValue::from_static("null"),
            );
        }
        None => {
            let result = state
                .services
                .objects
                .delete_detailed(&bucket_name, object_key)
                .await
                .map_err(|error| service_error(error, request_id, &format!("/{bucket}/{key}")))?;
            if let Some(marker) = result.delete_marker {
                response.headers_mut().insert(
                    HeaderName::from_static("x-amz-delete-marker"),
                    HeaderValue::from_static("true"),
                );
                insert_version_id(&mut response, marker.version_id);
            }
        }
    }
    Ok(response)
}

pub(crate) async fn post_object(
    State(state): State<S3State>,
    Path((bucket, key)): Path<(String, String)>,
    RawQuery(raw_query): RawQuery,
    Extension(request_id): Extension<S3RequestId>,
    Extension(payload_hash): Extension<PayloadHash>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, S3Error> {
    let query = query_map(raw_query.as_deref())
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &format!("/{bucket}/{key}")))?;
    let bucket_name = bucket_name(&bucket, &request_id)?;
    let object_key = object_key(&key, &request_id, &key)?;
    if query.contains_key("uploads") {
        if payload_hash
            .expected_checksum()
            .is_some_and(|checksum| checksum != Checksum::sha256(Sha256::digest([]).into()))
            || headers
                .get(header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .is_some_and(|length| length != 0)
        {
            return Err(S3Error::new(S3ErrorKind::BadDigest, request_id, &key));
        }
        let upload = state
            .services
            .objects
            .create_multipart(ServiceCreateMultipartRequest {
                bucket: bucket_name,
                key: object_key,
                content_type: headers
                    .get(header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned),
                custom_metadata: custom_metadata(
                    &headers,
                    &request_id,
                    &format!("/{bucket}/{key}"),
                )?,
                object_lock: requested_object_lock(
                    &headers,
                    &request_id,
                    &format!("/{bucket}/{key}"),
                )?,
            })
            .await
            .map_err(|error| service_error(error, request_id.clone(), &key))?;
        return xml_response(
            StatusCode::OK,
            &InitiateMultipartUploadResult {
                xmlns: "http://s3.amazonaws.com/doc/2006-03-01/",
                bucket,
                key,
                upload_id: upload.id.to_string(),
            },
            request_id,
            "/",
        );
    }
    let upload_id = query
        .get("uploadId")
        .ok_or_else(|| S3Error::new(S3ErrorKind::NotImplemented, request_id.clone(), &key))?
        .parse::<UploadId>()
        .map_err(|_| S3Error::new(S3ErrorKind::NoSuchUpload, request_id.clone(), &key))?;
    let bytes = read_verified(body, 1024 * 1024, &headers)
        .await
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &key))?;
    if payload_hash
        .expected_checksum()
        .is_some_and(|expected| expected != Checksum::sha256(Sha256::digest(bytes.as_ref()).into()))
    {
        return Err(S3Error::new(S3ErrorKind::BadDigest, request_id, &key));
    }
    let document: CompleteMultipartUploadDocument = quick_xml::de::from_reader(bytes.as_ref())
        .map_err(|_| S3Error::new(S3ErrorKind::MalformedXml, request_id.clone(), &key))?;
    let manifest = document
        .parts
        .into_iter()
        .map(|part| {
            Ok(CompletedPart {
                number: PartNumber::new(part.part_number).map_err(|_| {
                    S3Error::new(S3ErrorKind::InvalidPart, request_id.clone(), &key)
                })?,
                etag: ETag::new(part.etag.trim_matches('"').to_owned()).map_err(|_| {
                    S3Error::new(S3ErrorKind::InvalidPart, request_id.clone(), &key)
                })?,
            })
        })
        .collect::<Result<Vec<_>, S3Error>>()?;
    let result = state
        .services
        .objects
        .complete_multipart(ServiceCompleteMultipartRequest {
            bucket: bucket_name,
            key: object_key,
            upload_id,
            manifest,
        })
        .await
        .map_err(|error| service_error(error, request_id.clone(), &key))?;
    let document = CompleteMultipartUploadResult {
        xmlns: "http://s3.amazonaws.com/doc/2006-03-01/",
        location: format!("/{bucket}/{key}"),
        bucket,
        key,
        etag: format!("\"{}\"", result.metadata.etag),
        version_id: result.metadata.version_id.to_string(),
    };
    xml_response(StatusCode::OK, &document, request_id, &document.location)
}

pub(crate) async fn copy_object(
    state: S3State,
    bucket: String,
    key: String,
    request_id: S3RequestId,
    headers: HeaderMap,
) -> Result<Response, S3Error> {
    let source = headers
        .get("x-amz-copy-source")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), &key))?;
    let decoded = String::from_utf8(percent_decode_str(source).collect())
        .map_err(|_| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), &key))?;
    let (path, source_query) = decoded.split_once('?').unwrap_or((&decoded, ""));
    let (source_bucket, source_key) = path
        .trim_start_matches('/')
        .split_once('/')
        .ok_or_else(|| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), &key))?;
    let source_version_id = query_map(Some(source_query))
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &key))?
        .get("versionId")
        .map(|value| value.parse::<VersionId>())
        .transpose()
        .map_err(|_| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), &key))?;
    let directive = match headers
        .get("x-amz-metadata-directive")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("COPY")
    {
        "COPY" => CopyMetadataDirective::Copy,
        "REPLACE" => CopyMetadataDirective::Replace,
        _ => {
            return Err(S3Error::new(S3ErrorKind::InvalidRequest, request_id, &key));
        }
    };
    let result = state
        .services
        .objects
        .copy(ServiceCopyRequest {
            source_bucket: bucket_name(source_bucket, &request_id)?,
            source_key: object_key(source_key, &request_id, source_key)?,
            source_version_id,
            destination_bucket: bucket_name(&bucket, &request_id)?,
            destination_key: object_key(&key, &request_id, &key)?,
            metadata_directive: directive,
            content_type: headers
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
            replacement_metadata: custom_metadata(&headers, &request_id, &key)?,
        })
        .await
        .map_err(|error| service_error(error, request_id.clone(), &key))?;
    xml_response(
        StatusCode::OK,
        &CopyObjectResult {
            last_modified: result.metadata.modified_at.to_rfc3339(),
            etag: format!("\"{}\"", result.metadata.etag),
            version_id: result.metadata.version_id.to_string(),
        },
        request_id,
        &key,
    )
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderValue, Method, StatusCode, header};
    use tower::ServiceExt;

    use crate::test_support::*;
    use chrono::Utc;

    #[tokio::test]
    async fn signed_s3_lifecycle_streams_metadata_ranges_listing_and_idempotent_delete() {
        let (_directory, application, _credentials) = test_router().await;
        let now = Utc::now();

        let response = application
            .clone()
            .oneshot(signed_request(
                Method::GET,
                "/",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("list buckets response");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_text(response).await.contains("ListAllMyBucketsResult"));

        let response = application
            .clone()
            .oneshot(signed_request(
                Method::PUT,
                "/demo-bucket",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("create bucket response");
        assert_eq!(response.status(), StatusCode::OK);

        let duplicate = application
            .clone()
            .oneshot(signed_request(
                Method::PUT,
                "/demo-bucket",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("duplicate bucket response");
        assert_eq!(duplicate.status(), StatusCode::CONFLICT);
        assert_eq!(
            xml_value(&body_text(duplicate).await, "Code"),
            Some("BucketAlreadyExists")
        );

        let oversized_metadata = application
            .clone()
            .oneshot(signed_request(
                Method::PUT,
                "/demo-bucket/too-much-metadata",
                b"payload",
                &[
                    ("x-amz-meta-1", "value"),
                    ("x-amz-meta-2", "value"),
                    ("x-amz-meta-3", "value"),
                    ("x-amz-meta-4", "value"),
                    ("x-amz-meta-5", "value"),
                    ("x-amz-meta-6", "value"),
                    ("x-amz-meta-7", "value"),
                    ("x-amz-meta-8", "value"),
                    ("x-amz-meta-9", "value"),
                ],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("oversized metadata response");
        assert_eq!(oversized_metadata.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            xml_value(&body_text(oversized_metadata).await, "Code"),
            Some("InvalidRequest")
        );

        let object_uri = "/demo-bucket/users/123/profile.txt";
        let payload = b"hello world";
        let response = application
            .clone()
            .oneshot(signed_request(
                Method::PUT,
                object_uri,
                payload,
                &[
                    ("content-type", "text/plain"),
                    ("x-amz-meta-origin", "compatibility-test"),
                ],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("put object response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::ETAG),
            Some(&HeaderValue::from_static(
                "\"5eb63bbbe01eeed093cb22bb8f5acdc3\""
            ))
        );

        let copy = application
            .clone()
            .oneshot(signed_request(
                Method::PUT,
                "/demo-bucket/copied.txt",
                b"",
                &[("x-amz-copy-source", "/demo-bucket/users/123/profile.txt")],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("copy response");
        assert_eq!(copy.status(), StatusCode::OK);
        assert_eq!(
            xml_value(&body_text(copy).await, "ETag"),
            Some("\"5eb63bbbe01eeed093cb22bb8f5acdc3\"")
        );

        let head = application
            .clone()
            .oneshot(signed_request(
                Method::HEAD,
                object_uri,
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("head object response");
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(head.headers().get(header::CONTENT_LENGTH), Some(&11.into()));
        assert_eq!(
            head.headers().get(header::CONTENT_TYPE),
            Some(&HeaderValue::from_static("text/plain"))
        );
        assert_eq!(
            head.headers().get("x-amz-meta-origin"),
            Some(&HeaderValue::from_static("compatibility-test"))
        );

        let range = application
            .clone()
            .oneshot(signed_request(
                Method::GET,
                object_uri,
                b"",
                &[("range", "bytes=6-")],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("range response");
        assert_eq!(range.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            range.headers().get(header::CONTENT_RANGE),
            Some(&HeaderValue::from_static("bytes 6-10/11"))
        );
        assert_eq!(body_text(range).await, "world");

        let listing = application
            .clone()
            .oneshot(signed_request(
                Method::GET,
                "/demo-bucket?list-type=2&prefix=users%2F&delimiter=%2F&max-keys=1",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("list objects response");
        assert_eq!(listing.status(), StatusCode::OK);
        let listing = body_text(listing).await;
        assert!(listing.contains("<Prefix>users/123/</Prefix>"));
        assert!(listing.contains("<KeyCount>1</KeyCount>"));

        let non_empty = application
            .clone()
            .oneshot(signed_request(
                Method::DELETE,
                "/demo-bucket",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("non-empty bucket response");
        assert_eq!(non_empty.status(), StatusCode::CONFLICT);
        assert_eq!(
            xml_value(&body_text(non_empty).await, "Code"),
            Some("BucketNotEmpty")
        );

        let missing_multipart_delete = application
            .clone()
            .oneshot(signed_request(
                Method::DELETE,
                &format!("{object_uri}?uploadId=unsupported"),
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("missing multipart delete response");
        assert_eq!(missing_multipart_delete.status(), StatusCode::NOT_FOUND);

        for _ in 0..2 {
            let deleted = application
                .clone()
                .oneshot(signed_request(
                    Method::DELETE,
                    object_uri,
                    b"",
                    &[],
                    TEST_ACCESS_KEY,
                    TEST_SECRET_KEY,
                    now,
                ))
                .await
                .expect("delete object response");
            assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
        }

        let missing = application
            .clone()
            .oneshot(signed_request(
                Method::GET,
                object_uri,
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("missing object response");
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            xml_value(&body_text(missing).await, "Code"),
            Some("NoSuchKey")
        );

        let deleted_copy = application
            .clone()
            .oneshot(signed_request(
                Method::DELETE,
                "/demo-bucket/copied.txt",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("delete copied object response");
        assert_eq!(deleted_copy.status(), StatusCode::NO_CONTENT);

        let deleted_bucket = application
            .oneshot(signed_request(
                Method::DELETE,
                "/demo-bucket",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("delete bucket response");
        assert_eq!(deleted_bucket.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn multipart_and_versioning_work_through_signed_s3_requests() {
        let (_directory, application, _credentials) = test_router().await;
        let now = Utc::now();
        let create = application
            .clone()
            .oneshot(signed_request(
                Method::PUT,
                "/advanced-bucket",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("create bucket");
        assert_eq!(create.status(), StatusCode::OK);

        let configuration =
            b"<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>";
        let enabled = application
            .clone()
            .oneshot(signed_request(
                Method::PUT,
                "/advanced-bucket?versioning",
                configuration,
                &[("content-type", "application/xml")],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("enable versioning");
        assert_eq!(enabled.status(), StatusCode::OK);

        let first = application
            .clone()
            .oneshot(signed_request(
                Method::PUT,
                "/advanced-bucket/versioned.txt",
                b"first",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("first version");
        let first_version = first
            .headers()
            .get("x-amz-version-id")
            .expect("version header")
            .to_str()
            .expect("version text")
            .to_owned();
        assert_eq!(first.status(), StatusCode::OK);
        let second = application
            .clone()
            .oneshot(signed_request(
                Method::PUT,
                "/advanced-bucket/versioned.txt",
                b"second",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("second version");
        assert_eq!(second.status(), StatusCode::OK);
        let historical = application
            .clone()
            .oneshot(signed_request(
                Method::GET,
                &format!("/advanced-bucket/versioned.txt?versionId={first_version}"),
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("historical get");
        assert_eq!(historical.status(), StatusCode::OK);
        assert_eq!(body_text(historical).await, "first");
        let versions = application
            .clone()
            .oneshot(signed_request(
                Method::GET,
                "/advanced-bucket?versions",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("list versions");
        assert_eq!(versions.status(), StatusCode::OK);
        assert_eq!(body_text(versions).await.matches("<Version>").count(), 2);

        let initiated = application
            .clone()
            .oneshot(signed_request(
                Method::POST,
                "/advanced-bucket/multipart.bin?uploads",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("initiate multipart");
        assert_eq!(initiated.status(), StatusCode::OK);
        let upload_id = xml_value(&body_text(initiated).await, "UploadId")
            .expect("upload id")
            .to_owned();
        let part = application
            .clone()
            .oneshot(signed_request(
                Method::PUT,
                &format!("/advanced-bucket/multipart.bin?partNumber=1&uploadId={upload_id}"),
                b"streamed-part",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("upload part");
        assert_eq!(part.status(), StatusCode::OK);
        let etag = part
            .headers()
            .get(header::ETAG)
            .expect("part ETag")
            .to_str()
            .expect("ETag text")
            .to_owned();
        let completion = format!(
            "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{etag}</ETag></Part></CompleteMultipartUpload>"
        );
        let completed = application
            .clone()
            .oneshot(signed_request(
                Method::POST,
                &format!("/advanced-bucket/multipart.bin?uploadId={upload_id}"),
                completion.as_bytes(),
                &[("content-type", "application/xml")],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("complete multipart");
        assert_eq!(completed.status(), StatusCode::OK);
        let downloaded = application
            .oneshot(signed_request(
                Method::GET,
                "/advanced-bucket/multipart.bin",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("download multipart");
        assert_eq!(body_text(downloaded).await, "streamed-part");
    }

    /// Conditional requests are how a client caches. Each precondition has its
    /// own status, and getting one wrong makes a client either re-download
    /// everything or serve stale bytes.
    #[tokio::test]
    async fn conditional_reads_honour_the_stored_entity_tag() {
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;
        put(&application, "photos", "a.txt", b"hello").await;

        let head = send(&application, Method::HEAD, "/photos/a.txt", b"", &[]).await;
        assert_eq!(head.status(), StatusCode::OK);
        let etag = head
            .headers()
            .get("etag")
            .and_then(|value| value.to_str().ok())
            .expect("etag")
            .to_owned();

        let unmodified = send(
            &application,
            Method::GET,
            "/photos/a.txt",
            b"",
            &[("if-none-match", etag.as_str())],
        )
        .await;
        assert_eq!(unmodified.status(), StatusCode::NOT_MODIFIED);

        let matched = send(
            &application,
            Method::GET,
            "/photos/a.txt",
            b"",
            &[("if-match", etag.as_str())],
        )
        .await;
        assert_eq!(matched.status(), StatusCode::OK);

        let mismatched = send(
            &application,
            Method::GET,
            "/photos/a.txt",
            b"",
            &[("if-match", "\"0000\"")],
        )
        .await;
        assert_eq!(mismatched.status(), StatusCode::PRECONDITION_FAILED);
    }

    /// A range request must return only the requested bytes with the partial
    /// status, and an unsatisfiable range must say so rather than truncating.
    #[tokio::test]
    async fn range_requests_return_only_the_requested_bytes() {
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;
        put(&application, "photos", "a.txt", b"0123456789").await;

        let partial = send(
            &application,
            Method::GET,
            "/photos/a.txt",
            b"",
            &[("range", "bytes=2-5")],
        )
        .await;
        assert_eq!(partial.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(body_text(partial).await, "2345");

        let suffix = send(
            &application,
            Method::GET,
            "/photos/a.txt",
            b"",
            &[("range", "bytes=-3")],
        )
        .await;
        assert_eq!(suffix.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(body_text(suffix).await, "789");

        let impossible = send(
            &application,
            Method::GET,
            "/photos/a.txt",
            b"",
            &[("range", "bytes=100-200")],
        )
        .await;
        assert_eq!(impossible.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    }

    /// A client-supplied checksum is a promise about the bytes. Honouring a
    /// wrong one would store corruption under a checksum that says it is fine.
    #[tokio::test]
    async fn a_mismatched_client_checksum_refuses_the_write() {
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;

        let wrong = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [0_u8; 32]);
        let response = send(
            &application,
            Method::PUT,
            "/photos/a.txt",
            b"hello",
            &[("x-amz-checksum-sha256", wrong.as_str())],
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let document = body_text(response).await;
        assert_eq!(
            xml_value(&document, "Code"),
            Some("BadDigest"),
            "{document}"
        );

        let absent = send(&application, Method::HEAD, "/photos/a.txt", b"", &[]).await;
        assert_eq!(
            absent.status(),
            StatusCode::NOT_FOUND,
            "a refused write must store nothing"
        );
    }

    /// Every digest a client can send about its body is checked, not merely
    /// SHA-256 (RSG-007): a wrong one refuses the write with BadDigest and stores
    /// nothing, a right one stores the object and is echoed back, and an
    /// algorithm this server cannot verify is refused rather than ignored.
    #[tokio::test]
    async fn every_supplied_body_digest_is_verified_or_refused() {
        use base64::Engine as _;
        use sha2::Digest as _;
        let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;

        let md5_of_hello = encode(&md5::Md5::digest(b"hello"));
        let crc32_of_hello = encode(&crc32fast::hash(b"hello").to_be_bytes());
        let crc32c_of_hello = encode(&crc32c::crc32c(b"hello").to_be_bytes());
        let sha1_of_hello = encode(&sha1::Sha1::digest(b"hello"));
        let wrong_4 = encode(&[0_u8; 4]);
        let wrong_16 = encode(&[0_u8; 16]);
        let wrong_20 = encode(&[0_u8; 20]);
        let cases: [(&str, &str, &str); 4] = [
            ("content-md5", md5_of_hello.as_str(), wrong_16.as_str()),
            (
                "x-amz-checksum-crc32",
                crc32_of_hello.as_str(),
                wrong_4.as_str(),
            ),
            (
                "x-amz-checksum-crc32c",
                crc32c_of_hello.as_str(),
                wrong_4.as_str(),
            ),
            (
                "x-amz-checksum-sha1",
                sha1_of_hello.as_str(),
                wrong_20.as_str(),
            ),
        ];
        for (index, (header, right, wrong)) in cases.into_iter().enumerate() {
            let path = format!("/photos/wrong-{index}");
            let refused = send(
                &application,
                Method::PUT,
                &path,
                b"hello",
                &[(header, wrong)],
            )
            .await;
            assert_eq!(refused.status(), StatusCode::BAD_REQUEST, "{header}");
            let document = body_text(refused).await;
            assert_eq!(
                xml_value(&document, "Code"),
                Some("BadDigest"),
                "{header}: {document}"
            );
            let absent = send(&application, Method::HEAD, &path, b"", &[]).await;
            assert_eq!(
                absent.status(),
                StatusCode::NOT_FOUND,
                "{header}: a refused write stores nothing"
            );

            let path = format!("/photos/right-{index}");
            let stored = send(
                &application,
                Method::PUT,
                &path,
                b"hello",
                &[(header, right)],
            )
            .await;
            assert_eq!(stored.status(), StatusCode::OK, "{header}");
            if header.starts_with("x-amz-checksum-") {
                assert_eq!(
                    stored.headers().get(header).and_then(|v| v.to_str().ok()),
                    Some(right),
                    "{header} is echoed"
                );
            }
            let read = send(&application, Method::GET, &path, b"", &[]).await;
            assert_eq!(body_text(read).await, "hello");
        }

        let unverifiable = send(
            &application,
            Method::PUT,
            "/photos/crc64",
            b"hello",
            &[("x-amz-checksum-crc64nvme", "AAAAAAAAAAA=")],
        )
        .await;
        assert_eq!(unverifiable.status(), StatusCode::NOT_IMPLEMENTED);
        let absent = send(&application, Method::HEAD, "/photos/crc64", b"", &[]).await;
        assert_eq!(absent.status(), StatusCode::NOT_FOUND);
    }

    /// Custom metadata travels on `x-amz-meta-` headers and has to come back on
    /// the read, or a client loses information it believes it stored.
    #[tokio::test]
    async fn custom_metadata_and_content_type_survive_a_round_trip() {
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;

        let stored = send(
            &application,
            Method::PUT,
            "/photos/a.txt",
            b"hello",
            &[
                ("content-type", "text/plain"),
                ("x-amz-meta-owner", "finance"),
            ],
        )
        .await;
        assert_eq!(stored.status(), StatusCode::OK);

        let head = send(&application, Method::HEAD, "/photos/a.txt", b"", &[]).await;
        assert_eq!(
            head.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("text/plain")
        );
        assert_eq!(
            head.headers()
                .get("x-amz-meta-owner")
                .and_then(|v| v.to_str().ok()),
            Some("finance")
        );
    }

    /// Deleting is idempotent in S3: a client retrying a delete must not be told
    /// the object is missing, because that would look like a different failure.
    #[tokio::test]
    async fn deleting_an_object_twice_reports_success_both_times() {
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;
        put(&application, "photos", "a.txt", b"hello").await;

        for _ in 0..2 {
            let response = send(&application, Method::DELETE, "/photos/a.txt", b"", &[]).await;
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
        }
    }

    #[tokio::test]
    async fn reading_an_object_that_does_not_exist_reports_no_such_key() {
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;

        let response = send(&application, Method::GET, "/photos/absent.txt", b"", &[]).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let document = body_text(response).await;
        assert_eq!(
            xml_value(&document, "Code"),
            Some("NoSuchKey"),
            "{document}"
        );

        let head = send(&application, Method::HEAD, "/photos/absent.txt", b"", &[]).await;
        assert_eq!(
            head.status(),
            StatusCode::NOT_FOUND,
            "HEAD carries the status without a body"
        );
    }

    #[tokio::test]
    async fn writing_into_a_bucket_that_does_not_exist_reports_no_such_bucket() {
        let (_directory, application, _credentials) = test_router().await;
        let response = send(&application, Method::PUT, "/absent/a.txt", b"hello", &[]).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let document = body_text(response).await;
        assert_eq!(
            xml_value(&document, "Code"),
            Some("NoSuchBucket"),
            "{document}"
        );
    }

    /// A server-side copy must reproduce the bytes without the client streaming
    /// them, and must refuse a source that is not there.
    #[tokio::test]
    async fn a_copy_reproduces_the_source_and_refuses_a_missing_one() {
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;
        put(&application, "photos", "original.txt", b"source").await;

        let copied = send(
            &application,
            Method::PUT,
            "/photos/duplicate.txt",
            b"",
            &[("x-amz-copy-source", "/photos/original.txt")],
        )
        .await;
        assert_eq!(copied.status(), StatusCode::OK);

        let read = send(&application, Method::GET, "/photos/duplicate.txt", b"", &[]).await;
        assert_eq!(body_text(read).await, "source");

        let missing = send(
            &application,
            Method::PUT,
            "/photos/x.txt",
            b"",
            &[("x-amz-copy-source", "/photos/absent.txt")],
        )
        .await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }

    /// Aborting releases the parts. An upload that survives its abort keeps
    /// consuming storage nobody can see.
    #[tokio::test]
    async fn aborting_a_multipart_upload_makes_it_unusable() {
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;

        let initiated = send(
            &application,
            Method::POST,
            "/photos/big.bin?uploads",
            b"",
            &[],
        )
        .await;
        let document = body_text(initiated).await;
        let upload_id = xml_value(&document, "UploadId")
            .expect("upload id")
            .to_owned();

        let aborted = send(
            &application,
            Method::DELETE,
            &format!("/photos/big.bin?uploadId={upload_id}"),
            b"",
            &[],
        )
        .await;
        assert_eq!(aborted.status(), StatusCode::NO_CONTENT);

        let after = send(
            &application,
            Method::GET,
            &format!("/photos/big.bin?uploadId={upload_id}"),
            b"",
            &[],
        )
        .await;
        assert_eq!(after.status(), StatusCode::NOT_FOUND);
    }

    /// The whole point of multipart is assembling one object from parts. The
    /// completed object has to be the concatenation, in manifest order.
    #[tokio::test]
    async fn a_completed_multipart_upload_assembles_its_parts_in_order() {
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;

        let initiated = send(
            &application,
            Method::POST,
            "/photos/big.bin?uploads",
            b"",
            &[],
        )
        .await;
        assert_eq!(initiated.status(), StatusCode::OK);
        let document = body_text(initiated).await;
        let upload_id = xml_value(&document, "UploadId")
            .expect("upload id")
            .to_owned();

        let first = vec![b'a'; 5 * 1024 * 1024];
        let mut etags = Vec::new();
        for (number, body) in [(1_u16, first.as_slice()), (2, b"tail".as_slice())] {
            let response = send(
                &application,
                Method::PUT,
                &format!("/photos/big.bin?partNumber={number}&uploadId={upload_id}"),
                body,
                &[],
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK, "part {number}");
            let etag = response
                .headers()
                .get("etag")
                .and_then(|value| value.to_str().ok())
                .expect("etag")
                .trim_matches('"')
                .to_owned();
            etags.push((number, etag));
        }

        let manifest = etags
            .iter()
            .map(|(number, etag)| {
                format!("<Part><PartNumber>{number}</PartNumber><ETag>{etag}</ETag></Part>")
            })
            .collect::<String>();
        let completed = send(
            &application,
            Method::POST,
            &format!("/photos/big.bin?uploadId={upload_id}"),
            format!("<CompleteMultipartUpload>{manifest}</CompleteMultipartUpload>").as_bytes(),
            &[],
        )
        .await;
        assert_eq!(completed.status(), StatusCode::OK);

        let head = send(&application, Method::HEAD, "/photos/big.bin", b"", &[]).await;
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(
            head.headers()
                .get("content-length")
                .and_then(|value| value.to_str().ok()),
            Some((first.len() + 4).to_string().as_str()),
            "the object is the concatenation of its parts"
        );
    }

    /// A manifest naming a part that was never uploaded must not commit a
    /// partial object; the client would believe it stored bytes it did not.
    #[tokio::test]
    async fn completing_with_an_unknown_part_is_refused() {
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;
        let initiated = send(
            &application,
            Method::POST,
            "/photos/big.bin?uploads",
            b"",
            &[],
        )
        .await;
        let document = body_text(initiated).await;
        let upload_id = xml_value(&document, "UploadId")
            .expect("upload id")
            .to_owned();

        let response = send(
            &application,
            Method::POST,
            &format!("/photos/big.bin?uploadId={upload_id}"),
            b"<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>00000000000000000000000000000000</ETag></Part></CompleteMultipartUpload>",
            &[],
        )
        .await;
        assert!(
            response.status().is_client_error(),
            "an unknown part must not complete: {}",
            response.status()
        );

        let head = send(&application, Method::HEAD, "/photos/big.bin", b"", &[]).await;
        assert_eq!(
            head.status(),
            StatusCode::NOT_FOUND,
            "nothing may be committed"
        );
    }

    /// With versioning on, a specific version stays readable and individually
    /// deletable through the `versionId` subresource.
    #[tokio::test]
    async fn a_specific_version_can_be_read_and_deleted() {
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;
        let enabled = send(
            &application,
            Method::PUT,
            "/photos?versioning",
            b"<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>",
            &[],
        )
        .await;
        assert_eq!(enabled.status(), StatusCode::OK);

        let first = send(&application, Method::PUT, "/photos/note.txt", b"one", &[]).await;
        let version = first
            .headers()
            .get("x-amz-version-id")
            .and_then(|value| value.to_str().ok())
            .expect("version id")
            .to_owned();
        put(&application, "photos", "note.txt", b"two").await;

        let historical = send(
            &application,
            Method::GET,
            &format!("/photos/note.txt?versionId={version}"),
            b"",
            &[],
        )
        .await;
        assert_eq!(historical.status(), StatusCode::OK);
        assert_eq!(body_text(historical).await, "one");

        let removed = send(
            &application,
            Method::DELETE,
            &format!("/photos/note.txt?versionId={version}"),
            b"",
            &[],
        )
        .await;
        assert_eq!(removed.status(), StatusCode::NO_CONTENT);

        let gone = send(
            &application,
            Method::GET,
            &format!("/photos/note.txt?versionId={version}"),
            b"",
            &[],
        )
        .await;
        assert_eq!(gone.status(), StatusCode::NOT_FOUND);
        let current = send(&application, Method::GET, "/photos/note.txt", b"", &[]).await;
        assert_eq!(
            body_text(current).await,
            "two",
            "the current version is untouched"
        );
    }

    /// A copy can either carry the source's metadata across or replace it. The
    /// directive decides, and getting it wrong silently changes an object's type.
    #[tokio::test]
    async fn a_copy_honours_the_metadata_directive() {
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;
        let stored = send(
            &application,
            Method::PUT,
            "/photos/original.txt",
            b"source",
            &[("content-type", "text/plain")],
        )
        .await;
        assert_eq!(stored.status(), StatusCode::OK);

        let carried = send(
            &application,
            Method::PUT,
            "/photos/carried.txt",
            b"",
            &[("x-amz-copy-source", "/photos/original.txt")],
        )
        .await;
        assert_eq!(carried.status(), StatusCode::OK);
        let head = send(&application, Method::HEAD, "/photos/carried.txt", b"", &[]).await;
        assert_eq!(
            head.headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("text/plain"),
            "COPY carries the source's type"
        );

        let replaced = send(
            &application,
            Method::PUT,
            "/photos/replaced.txt",
            b"",
            &[
                ("x-amz-copy-source", "/photos/original.txt"),
                ("x-amz-metadata-directive", "REPLACE"),
                ("content-type", "application/json"),
            ],
        )
        .await;
        assert_eq!(replaced.status(), StatusCode::OK);
        let head = send(&application, Method::HEAD, "/photos/replaced.txt", b"", &[]).await;
        assert_eq!(
            head.headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("application/json"),
            "REPLACE uses the supplied type"
        );
    }

    /// An unsupported subresource is reported as `NotImplemented` rather than
    /// being silently treated as an ordinary read, which would return the object
    /// while ignoring what the client actually asked for.
    #[tokio::test]
    async fn an_unsupported_object_subresource_reports_not_implemented() {
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;
        put(&application, "photos", "a.txt", b"hello").await;

        for uri in [
            "/photos/a.txt?acl",
            "/photos/a.txt?tagging",
            "/photos/a.txt?torrent",
            "/photos/a.txt?restore",
        ] {
            let response = send(&application, Method::GET, uri, b"", &[]).await;
            assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED, "{uri}");
            let document = body_text(response).await;
            assert_eq!(
                xml_value(&document, "Code"),
                Some("NotImplemented"),
                "{document}"
            );
        }
    }

    /// `?retention` and `?legal-hold` are implemented now, so they must not be
    /// reported as unimplemented — but on a bucket without Object Lock they
    /// still have to fail rather than answer as though a lock existed.
    #[tokio::test]
    async fn lock_subresources_on_an_unlocked_bucket_are_refused_rather_than_answered() {
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;
        put(&application, "photos", "a.txt", b"hello").await;

        for uri in ["/photos/a.txt?retention", "/photos/a.txt?legal-hold"] {
            let response = send(&application, Method::GET, uri, b"", &[]).await;
            assert_ne!(
                response.status(),
                StatusCode::NOT_IMPLEMENTED,
                "{uri} is implemented"
            );
            assert!(response.status().is_client_error(), "{uri}");
            let document = body_text(response).await;
            assert_eq!(
                xml_value(&document, "Code"),
                Some("InvalidRequest"),
                "{document}"
            );
        }
    }
}

/// Builds the Object Lock decision context for one request.
fn lock_context(principal: Option<&Extension<Principal>>, headers: &HeaderMap) -> LockContext {
    LockContext::principal(principal_name(principal.map(|extension| &extension.0)))
        .with_governance_bypass(requested_governance_bypass(headers))
}

/// Resolves the version a lock subresource names, or the current one.
fn requested_lock_version(
    query: &std::collections::BTreeMap<String, String>,
    request_id: &S3RequestId,
    resource: &str,
) -> Result<Option<VersionId>, S3Error> {
    match requested_version(query)
        .map_err(|kind| S3Error::new(kind, request_id.clone(), resource))?
    {
        Some(RequestedVersion::Id(version_id)) => Ok(Some(version_id)),
        // The special null version belongs to a bucket that is not
        // version-enabled, and Object Lock requires versioning, so a lock
        // request can never legitimately name it.
        Some(RequestedVersion::Null) => Err(S3Error::new(
            S3ErrorKind::ObjectLockNotEnabled,
            request_id.clone(),
            resource,
        )),
        None => Ok(None),
    }
}

fn lock_key_parts(
    bucket: &str,
    key: &str,
    request_id: &S3RequestId,
) -> Result<(record_store_core::BucketName, CoreObjectKey), S3Error> {
    let resource = format!("/{bucket}/{key}");
    Ok((
        bucket_name(bucket, request_id)?,
        object_key(key, request_id, &resource)?,
    ))
}

#[expect(
    clippy::too_many_arguments,
    reason = "an axum handler's inputs are its extractors; grouping them would only \
              move the same values behind another type"
)]
pub(crate) async fn put_object_retention(
    state: S3State,
    bucket: String,
    key: String,
    query: &std::collections::BTreeMap<String, String>,
    request_id: S3RequestId,
    principal: Option<Extension<Principal>>,
    headers: &HeaderMap,
    body: Body,
) -> Result<Response, S3Error> {
    let resource = format!("/{bucket}/{key}");
    let (bucket_name, object_key) = lock_key_parts(&bucket, &key, &request_id)?;
    let version_id = requested_lock_version(query, &request_id, &resource)?;
    let bytes = read_verified(body, 16 * 1024, headers)
        .await
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &resource))?;
    let document: RetentionDocument = quick_xml::de::from_reader(bytes.as_ref())
        .map_err(|_| S3Error::new(S3ErrorKind::MalformedXml, request_id.clone(), &resource))?;
    let retention: Option<record_store_core::Retention> = document
        .try_into()
        .map_err(|_| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), &resource))?;
    let context = lock_context(principal.as_ref(), headers);
    let result = state
        .services
        .locks
        .put_retention(&bucket_name, &object_key, version_id, retention, &context)
        .await
        .map_err(|error| service_error(error, request_id.clone(), &resource))?;
    let mut response = StatusCode::OK.into_response();
    insert_version_id(&mut response, result.version_id);
    Ok(response)
}

pub(crate) async fn get_object_retention(
    state: S3State,
    bucket: String,
    key: String,
    query: &std::collections::BTreeMap<String, String>,
    request_id: S3RequestId,
) -> Result<Response, S3Error> {
    let resource = format!("/{bucket}/{key}");
    let (bucket_name, object_key) = lock_key_parts(&bucket, &key, &request_id)?;
    let version_id = requested_lock_version(query, &request_id, &resource)?;
    let result = state
        .services
        .locks
        .get(&bucket_name, &object_key, version_id)
        .await
        .map_err(|error| service_error(error, request_id.clone(), &resource))?;
    // A version that carries no retention has no document to return, which is
    // a different answer from a retention of zero and is reported as such.
    let retention = result.state.retention.ok_or_else(|| {
        S3Error::new(
            S3ErrorKind::NoSuchObjectLockConfiguration,
            request_id.clone(),
            &resource,
        )
    })?;
    let mut response = xml_response(
        StatusCode::OK,
        &RetentionResult {
            xmlns: "http://s3.amazonaws.com/doc/2006-03-01/",
            mode: retention.mode.as_str(),
            retain_until_date: format_retain_until(retention.retain_until),
        },
        request_id,
        &resource,
    )?;
    insert_version_id(&mut response, result.version_id);
    Ok(response)
}

#[expect(
    clippy::too_many_arguments,
    reason = "an axum handler's inputs are its extractors; grouping them would only \
              move the same values behind another type"
)]
pub(crate) async fn put_object_legal_hold(
    state: S3State,
    bucket: String,
    key: String,
    query: &std::collections::BTreeMap<String, String>,
    request_id: S3RequestId,
    principal: Option<Extension<Principal>>,
    headers: &HeaderMap,
    body: Body,
) -> Result<Response, S3Error> {
    let resource = format!("/{bucket}/{key}");
    let (bucket_name, object_key) = lock_key_parts(&bucket, &key, &request_id)?;
    let version_id = requested_lock_version(query, &request_id, &resource)?;
    let bytes = read_verified(body, 16 * 1024, headers)
        .await
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &resource))?;
    let document: LegalHoldDocument = quick_xml::de::from_reader(bytes.as_ref())
        .map_err(|_| S3Error::new(S3ErrorKind::MalformedXml, request_id.clone(), &resource))?;
    let legal_hold = parse_legal_hold(document.status.as_deref().unwrap_or_default())
        .map_err(|_| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), &resource))?;
    let context = lock_context(principal.as_ref(), headers);
    let result = state
        .services
        .locks
        .put_legal_hold(&bucket_name, &object_key, version_id, legal_hold, &context)
        .await
        .map_err(|error| service_error(error, request_id.clone(), &resource))?;
    let mut response = StatusCode::OK.into_response();
    insert_version_id(&mut response, result.version_id);
    Ok(response)
}

pub(crate) async fn get_object_legal_hold(
    state: S3State,
    bucket: String,
    key: String,
    query: &std::collections::BTreeMap<String, String>,
    request_id: S3RequestId,
) -> Result<Response, S3Error> {
    let resource = format!("/{bucket}/{key}");
    let (bucket_name, object_key) = lock_key_parts(&bucket, &key, &request_id)?;
    let version_id = requested_lock_version(query, &request_id, &resource)?;
    let result = state
        .services
        .locks
        .get(&bucket_name, &object_key, version_id)
        .await
        .map_err(|error| service_error(error, request_id.clone(), &resource))?;
    // Unlike retention, a legal hold is reported as OFF rather than as an
    // absent document: the question "is this version held" always has an
    // answer once the bucket has Object Lock enabled.
    let mut response = xml_response(
        StatusCode::OK,
        &LegalHoldResult {
            xmlns: "http://s3.amazonaws.com/doc/2006-03-01/",
            status: legal_hold_status(result.state.legal_hold),
        },
        request_id,
        &resource,
    )?;
    insert_version_id(&mut response, result.version_id);
    Ok(response)
}

/// Adds the Object Lock headers to a read response, when the version has any.
async fn attach_object_lock_headers(
    state: &S3State,
    response: &mut Response,
    version_id: VersionId,
) {
    match state.services.objects.version_lock(version_id).await {
        Ok(lock) if !lock.is_unlocked() => apply_object_lock_headers(response, &lock),
        Ok(_) => {}
        // A read must not fail because the lock annotation could not be read;
        // the payload and its metadata are still correct. The omission is
        // logged rather than silently swallowed.
        Err(error) => tracing::warn!(%error, %version_id, "object lock headers omitted from read"),
    }
}

#[cfg(test)]
mod object_lock_tests {
    use axum::http::{Method, StatusCode};
    use chrono::{Duration, Utc};

    use crate::test_support::*;

    fn retain_until(days: i64) -> String {
        (Utc::now() + Duration::days(days)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    fn retention_document(mode: &str, days: i64) -> String {
        format!(
            "<Retention><Mode>{mode}</Mode><RetainUntilDate>{}</RetainUntilDate></Retention>",
            retain_until(days)
        )
    }

    /// Object Lock brings versioning with it and cannot be turned on later, so
    /// a bucket either starts locked or never is.
    #[tokio::test]
    async fn creating_a_bucket_with_object_lock_enables_versioning_and_reports_the_configuration() {
        let (_directory, application, _credentials) = test_router().await;
        make_locked_bucket(&application, "records").await;

        let versioning = send(&application, Method::GET, "/records?versioning", b"", &[]).await;
        let document = body_text(versioning).await;
        assert_eq!(
            xml_value(&document, "Status"),
            Some("Enabled"),
            "object lock implies versioning: {document}"
        );

        let configuration = send(&application, Method::GET, "/records?object-lock", b"", &[]).await;
        assert_eq!(configuration.status(), StatusCode::OK);
        let document = body_text(configuration).await;
        assert_eq!(
            xml_value(&document, "ObjectLockEnabled"),
            Some("Enabled"),
            "{document}"
        );
    }

    #[tokio::test]
    async fn an_unlocked_bucket_reports_that_object_lock_was_never_enabled() {
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;

        let response = send(&application, Method::GET, "/photos?object-lock", b"", &[]).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let document = body_text(response).await;
        assert_eq!(
            xml_value(&document, "Code"),
            Some("ObjectLockConfigurationNotFoundError"),
            "{document}"
        );
    }

    /// Enabling lock on an existing bucket would claim protection over versions
    /// written without it, so it is refused rather than quietly accepted.
    #[tokio::test]
    async fn object_lock_cannot_be_enabled_on_an_existing_bucket() {
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;

        let response = send(
            &application,
            Method::PUT,
            "/photos?object-lock",
            b"<ObjectLockConfiguration><ObjectLockEnabled>Enabled</ObjectLockEnabled></ObjectLockConfiguration>",
            &[],
        )
        .await;
        assert!(response.status().is_client_error(), "{}", response.status());
        let document = body_text(response).await;
        assert_eq!(
            xml_value(&document, "Code"),
            Some("InvalidRequest"),
            "{document}"
        );
    }

    #[tokio::test]
    async fn a_bucket_default_retention_round_trips_and_applies_to_new_versions() {
        let (_directory, application, _credentials) = test_router().await;
        make_locked_bucket(&application, "records").await;

        let applied = send(
            &application,
            Method::PUT,
            "/records?object-lock",
            b"<ObjectLockConfiguration><ObjectLockEnabled>Enabled</ObjectLockEnabled><Rule><DefaultRetention><Mode>GOVERNANCE</Mode><Days>30</Days></DefaultRetention></Rule></ObjectLockConfiguration>",
            &[],
        )
        .await;
        assert_eq!(applied.status(), StatusCode::OK);

        let read = send(&application, Method::GET, "/records?object-lock", b"", &[]).await;
        let document = body_text(read).await;
        assert_eq!(
            xml_value(&document, "Mode"),
            Some("GOVERNANCE"),
            "{document}"
        );
        assert_eq!(xml_value(&document, "Days"), Some("30"), "{document}");

        // A version written afterwards carries the default, materialized onto it.
        put_returning_version(&application, "records", "a.txt", b"hello", &[]).await;
        let head = send(&application, Method::HEAD, "/records/a.txt", b"", &[]).await;
        assert_eq!(
            response_header(&head, "x-amz-object-lock-mode").as_deref(),
            Some("GOVERNANCE"),
            "a bucket default materializes onto the version"
        );
        assert!(response_header(&head, "x-amz-object-lock-retain-until-date").is_some());
    }

    #[tokio::test]
    async fn a_default_retention_naming_both_days_and_years_is_refused() {
        let (_directory, application, _credentials) = test_router().await;
        make_locked_bucket(&application, "records").await;
        let response = send(
            &application,
            Method::PUT,
            "/records?object-lock",
            b"<ObjectLockConfiguration><ObjectLockEnabled>Enabled</ObjectLockEnabled><Rule><DefaultRetention><Mode>GOVERNANCE</Mode><Days>30</Days><Years>1</Years></DefaultRetention></Rule></ObjectLockConfiguration>",
            &[],
        )
        .await;
        assert!(response.status().is_client_error(), "{}", response.status());
    }

    /// The write headers and the read headers have to agree, or a client cannot
    /// confirm what it asked for actually took effect.
    #[tokio::test]
    async fn put_object_lock_headers_are_reported_back_on_get_and_head() {
        let (_directory, application, _credentials) = test_router().await;
        make_locked_bucket(&application, "records").await;
        let until = retain_until(10);

        put_returning_version(
            &application,
            "records",
            "a.txt",
            b"hello",
            &[
                ("x-amz-object-lock-mode", "COMPLIANCE"),
                ("x-amz-object-lock-retain-until-date", &until),
                ("x-amz-object-lock-legal-hold", "ON"),
            ],
        )
        .await;

        for method in [Method::GET, Method::HEAD] {
            let response = send(&application, method.clone(), "/records/a.txt", b"", &[]).await;
            assert_eq!(response.status(), StatusCode::OK, "{method}");
            assert_eq!(
                response_header(&response, "x-amz-object-lock-mode").as_deref(),
                Some("COMPLIANCE"),
                "{method}"
            );
            assert_eq!(
                response_header(&response, "x-amz-object-lock-retain-until-date").as_deref(),
                Some(until.as_str()),
                "{method}"
            );
            assert_eq!(
                response_header(&response, "x-amz-object-lock-legal-hold").as_deref(),
                Some("ON"),
                "{method}"
            );
        }
    }

    /// A mode with no date, or a date with no mode, describes no retention at
    /// all. Accepting half of one would promise a protection nobody asked for.
    #[tokio::test]
    async fn half_a_retention_header_pair_is_refused() {
        let (_directory, application, _credentials) = test_router().await;
        make_locked_bucket(&application, "records").await;

        for headers in [
            vec![("x-amz-object-lock-mode", "COMPLIANCE")],
            vec![(
                "x-amz-object-lock-retain-until-date",
                retain_until(1).leak() as &str,
            )],
        ] {
            let response = send(
                &application,
                Method::PUT,
                "/records/partial.txt",
                b"hello",
                &headers,
            )
            .await;
            assert!(
                response.status().is_client_error(),
                "{headers:?}: {}",
                response.status()
            );
        }
    }

    #[tokio::test]
    async fn lock_headers_on_a_bucket_without_object_lock_are_refused() {
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;

        let response = send(
            &application,
            Method::PUT,
            "/photos/a.txt",
            b"hello",
            &[
                ("x-amz-object-lock-mode", "GOVERNANCE"),
                ("x-amz-object-lock-retain-until-date", &retain_until(1)),
            ],
        )
        .await;
        assert!(response.status().is_client_error(), "{}", response.status());
        let document = body_text(response).await;
        assert_eq!(
            xml_value(&document, "Code"),
            Some("InvalidRequest"),
            "{document}"
        );
    }

    /// An `x-amz-object-lock-*` header this adapter does not model is still an
    /// unimplemented semantic and must not be silently dropped.
    #[tokio::test]
    async fn an_unmodelled_object_lock_header_is_still_not_implemented() {
        let (_directory, application, _credentials) = test_router().await;
        make_locked_bucket(&application, "records").await;
        let response = send(
            &application,
            Method::PUT,
            "/records/a.txt",
            b"hello",
            &[("x-amz-object-lock-token", "something")],
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn retention_round_trips_through_its_subresource() {
        let (_directory, application, _credentials) = test_router().await;
        make_locked_bucket(&application, "records").await;
        let version = put_returning_version(&application, "records", "a.txt", b"hello", &[]).await;

        let absent = send(
            &application,
            Method::GET,
            "/records/a.txt?retention",
            b"",
            &[],
        )
        .await;
        assert_eq!(absent.status(), StatusCode::NOT_FOUND);
        let document = body_text(absent).await;
        assert_eq!(
            xml_value(&document, "Code"),
            Some("NoSuchObjectLockConfiguration"),
            "an unretained version has no retention document: {document}"
        );

        let applied = send(
            &application,
            Method::PUT,
            &format!("/records/a.txt?retention&versionId={version}"),
            retention_document("GOVERNANCE", 10).as_bytes(),
            &[],
        )
        .await;
        assert_eq!(applied.status(), StatusCode::OK);

        let read = send(
            &application,
            Method::GET,
            "/records/a.txt?retention",
            b"",
            &[],
        )
        .await;
        assert_eq!(read.status(), StatusCode::OK);
        let document = body_text(read).await;
        assert_eq!(
            xml_value(&document, "Mode"),
            Some("GOVERNANCE"),
            "{document}"
        );
    }

    #[tokio::test]
    async fn a_legal_hold_round_trips_and_reports_off_when_never_placed() {
        let (_directory, application, _credentials) = test_router().await;
        make_locked_bucket(&application, "records").await;
        put_returning_version(&application, "records", "a.txt", b"hello", &[]).await;

        let initial = send(
            &application,
            Method::GET,
            "/records/a.txt?legal-hold",
            b"",
            &[],
        )
        .await;
        assert_eq!(initial.status(), StatusCode::OK);
        let document = body_text(initial).await;
        assert_eq!(xml_value(&document, "Status"), Some("OFF"), "{document}");

        for status in ["ON", "OFF"] {
            let applied = send(
                &application,
                Method::PUT,
                "/records/a.txt?legal-hold",
                format!("<LegalHold><Status>{status}</Status></LegalHold>").as_bytes(),
                &[],
            )
            .await;
            assert_eq!(applied.status(), StatusCode::OK, "set {status}");

            let read = send(
                &application,
                Method::GET,
                "/records/a.txt?legal-hold",
                b"",
                &[],
            )
            .await;
            let document = body_text(read).await;
            assert_eq!(xml_value(&document, "Status"), Some(status), "{document}");
        }
    }

    #[tokio::test]
    async fn a_legal_hold_status_that_is_not_on_or_off_is_refused() {
        let (_directory, application, _credentials) = test_router().await;
        make_locked_bucket(&application, "records").await;
        put_returning_version(&application, "records", "a.txt", b"hello", &[]).await;
        let response = send(
            &application,
            Method::PUT,
            "/records/a.txt?legal-hold",
            b"<LegalHold><Status>MAYBE</Status></LegalHold>",
            &[],
        )
        .await;
        assert!(response.status().is_client_error(), "{}", response.status());
    }

    /// The headline rule. A compliance retention holds against the credential
    /// that owns the deployment, which is the only thing that makes the mode
    /// mean anything.
    #[tokio::test]
    async fn a_compliance_retention_refuses_deletion_and_shortening_even_with_a_bypass() {
        let (_directory, application, _credentials) = test_router().await;
        make_locked_bucket(&application, "records").await;
        let version = put_returning_version(
            &application,
            "records",
            "statement.pdf",
            b"hello",
            &[
                ("x-amz-object-lock-mode", "COMPLIANCE"),
                ("x-amz-object-lock-retain-until-date", &retain_until(30)),
            ],
        )
        .await;

        let deleted = send(
            &application,
            Method::DELETE,
            &format!("/records/statement.pdf?versionId={version}"),
            b"",
            &[("x-amz-bypass-governance-retention", "true")],
        )
        .await;
        assert_eq!(deleted.status(), StatusCode::FORBIDDEN);
        let document = body_text(deleted).await;
        assert_eq!(
            xml_value(&document, "Code"),
            Some("AccessDenied"),
            "{document}"
        );

        let shortened = send(
            &application,
            Method::PUT,
            &format!("/records/statement.pdf?retention&versionId={version}"),
            retention_document("COMPLIANCE", 1).as_bytes(),
            &[("x-amz-bypass-governance-retention", "true")],
        )
        .await;
        assert_eq!(shortened.status(), StatusCode::FORBIDDEN);

        // The object is still readable, which is the point of retaining it.
        let read = send(
            &application,
            Method::GET,
            &format!("/records/statement.pdf?versionId={version}"),
            b"",
            &[],
        )
        .await;
        assert_eq!(read.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_compliance_retention_may_still_be_extended() {
        let (_directory, application, _credentials) = test_router().await;
        make_locked_bucket(&application, "records").await;
        let version = put_returning_version(
            &application,
            "records",
            "statement.pdf",
            b"hello",
            &[
                ("x-amz-object-lock-mode", "COMPLIANCE"),
                ("x-amz-object-lock-retain-until-date", &retain_until(30)),
            ],
        )
        .await;
        let extended = send(
            &application,
            Method::PUT,
            &format!("/records/statement.pdf?retention&versionId={version}"),
            retention_document("COMPLIANCE", 60).as_bytes(),
            &[],
        )
        .await;
        assert_eq!(extended.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_governance_retention_refuses_a_delete_until_a_bypass_is_presented() {
        let (_directory, application, _credentials) = test_router().await;
        make_locked_bucket(&application, "records").await;
        let version = put_returning_version(
            &application,
            "records",
            "draft.txt",
            b"hello",
            &[
                ("x-amz-object-lock-mode", "GOVERNANCE"),
                ("x-amz-object-lock-retain-until-date", &retain_until(30)),
            ],
        )
        .await;

        let refused = send(
            &application,
            Method::DELETE,
            &format!("/records/draft.txt?versionId={version}"),
            b"",
            &[],
        )
        .await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);

        let bypassed = send(
            &application,
            Method::DELETE,
            &format!("/records/draft.txt?versionId={version}"),
            b"",
            &[("x-amz-bypass-governance-retention", "true")],
        )
        .await;
        assert_eq!(bypassed.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn a_legal_hold_blocks_deletion_independently_of_any_retention() {
        let (_directory, application, _credentials) = test_router().await;
        make_locked_bucket(&application, "records").await;
        let version = put_returning_version(
            &application,
            "records",
            "held.txt",
            b"hello",
            &[("x-amz-object-lock-legal-hold", "ON")],
        )
        .await;

        let refused = send(
            &application,
            Method::DELETE,
            &format!("/records/held.txt?versionId={version}"),
            b"",
            &[("x-amz-bypass-governance-retention", "true")],
        )
        .await;
        assert_eq!(
            refused.status(),
            StatusCode::FORBIDDEN,
            "no bypass applies to a legal hold"
        );

        // Removing the hold is what unblocks it, not a bypass.
        let released = send(
            &application,
            Method::PUT,
            &format!("/records/held.txt?legal-hold&versionId={version}"),
            b"<LegalHold><Status>OFF</Status></LegalHold>",
            &[],
        )
        .await;
        assert_eq!(released.status(), StatusCode::OK);
        let deleted = send(
            &application,
            Method::DELETE,
            &format!("/records/held.txt?versionId={version}"),
            b"",
            &[],
        )
        .await;
        assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    }

    /// Deleting the *version* is refused; hiding the object behind a delete
    /// marker is not. This is the distinction the documentation calls out, and
    /// clients depend on both halves of it.
    #[tokio::test]
    async fn a_delete_marker_is_allowed_while_the_retained_version_is_not_deletable() {
        let (_directory, application, _credentials) = test_router().await;
        make_locked_bucket(&application, "records").await;
        let version = put_returning_version(
            &application,
            "records",
            "statement.pdf",
            b"hello",
            &[
                ("x-amz-object-lock-mode", "COMPLIANCE"),
                ("x-amz-object-lock-retain-until-date", &retain_until(30)),
            ],
        )
        .await;

        let marker = send(
            &application,
            Method::DELETE,
            "/records/statement.pdf",
            b"",
            &[],
        )
        .await;
        assert_eq!(marker.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            response_header(&marker, "x-amz-delete-marker").as_deref(),
            Some("true")
        );

        // The retained version is still readable by its identifier.
        let read = send(
            &application,
            Method::GET,
            &format!("/records/statement.pdf?versionId={version}"),
            b"",
            &[],
        )
        .await;
        assert_eq!(read.status(), StatusCode::OK);
        assert_eq!(body_text(read).await, "hello");

        // And still refuses to be removed.
        let deleted = send(
            &application,
            Method::DELETE,
            &format!("/records/statement.pdf?versionId={version}"),
            b"",
            &[],
        )
        .await;
        assert_eq!(deleted.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn overwriting_a_key_never_mutates_the_locked_version() {
        let (_directory, application, _credentials) = test_router().await;
        make_locked_bucket(&application, "records").await;
        let first = put_returning_version(
            &application,
            "records",
            "statement.pdf",
            b"original",
            &[
                ("x-amz-object-lock-mode", "COMPLIANCE"),
                ("x-amz-object-lock-retain-until-date", &retain_until(30)),
            ],
        )
        .await;
        let second = put_returning_version(
            &application,
            "records",
            "statement.pdf",
            b"replacement",
            &[],
        )
        .await;
        assert_ne!(first, second, "an overwrite publishes a new version");

        let original = send(
            &application,
            Method::GET,
            &format!("/records/statement.pdf?versionId={first}"),
            b"",
            &[],
        )
        .await;
        assert_eq!(original.status(), StatusCode::OK);
        assert_eq!(
            response_header(&original, "x-amz-object-lock-mode").as_deref(),
            Some("COMPLIANCE"),
            "the locked version keeps its retention"
        );
        assert_eq!(body_text(original).await, "original");
    }

    #[tokio::test]
    async fn versioning_cannot_be_suspended_on_a_locked_bucket() {
        let (_directory, application, _credentials) = test_router().await;
        make_locked_bucket(&application, "records").await;
        let response = send(
            &application,
            Method::PUT,
            "/records?versioning",
            b"<VersioningConfiguration><Status>Suspended</Status></VersioningConfiguration>",
            &[],
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let document = body_text(response).await;
        assert_eq!(
            xml_value(&document, "Code"),
            Some("InvalidBucketState"),
            "{document}"
        );
    }

    #[tokio::test]
    async fn multipart_uploads_carry_the_lock_chosen_at_initiation() {
        let (_directory, application, _credentials) = test_router().await;
        make_locked_bucket(&application, "records").await;
        let until = retain_until(20);

        let initiated = send(
            &application,
            Method::POST,
            "/records/large.bin?uploads",
            b"",
            &[
                ("x-amz-object-lock-mode", "GOVERNANCE"),
                ("x-amz-object-lock-retain-until-date", &until),
            ],
        )
        .await;
        assert_eq!(initiated.status(), StatusCode::OK);
        let document = body_text(initiated).await;
        let upload_id = xml_value(&document, "UploadId")
            .expect("upload id")
            .to_owned();

        let part = send(
            &application,
            Method::PUT,
            &format!("/records/large.bin?uploadId={upload_id}&partNumber=1"),
            b"multipart payload",
            &[],
        )
        .await;
        assert_eq!(part.status(), StatusCode::OK);
        let etag = response_header(&part, "etag").expect("part etag");
        let etag = etag.trim_matches('"');

        let completed = send(
            &application,
            Method::POST,
            &format!("/records/large.bin?uploadId={upload_id}"),
            format!(
                "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>\"{etag}\"</ETag></Part></CompleteMultipartUpload>"
            )
            .as_bytes(),
            &[],
        )
        .await;
        assert_eq!(completed.status(), StatusCode::OK);

        let head = send(&application, Method::HEAD, "/records/large.bin", b"", &[]).await;
        assert_eq!(
            response_header(&head, "x-amz-object-lock-mode").as_deref(),
            Some("GOVERNANCE"),
            "the lock chosen at initiation reaches the completed version"
        );
        assert_eq!(
            response_header(&head, "x-amz-object-lock-retain-until-date").as_deref(),
            Some(until.as_str())
        );
    }

    #[tokio::test]
    async fn an_unlocked_version_reports_no_object_lock_headers() {
        let (_directory, application, _credentials) = test_router().await;
        make_locked_bucket(&application, "records").await;
        put_returning_version(&application, "records", "plain.txt", b"hello", &[]).await;

        let head = send(&application, Method::HEAD, "/records/plain.txt", b"", &[]).await;
        assert_eq!(head.status(), StatusCode::OK);
        for header in [
            "x-amz-object-lock-mode",
            "x-amz-object-lock-retain-until-date",
            "x-amz-object-lock-legal-hold",
        ] {
            assert!(
                response_header(&head, header).is_none(),
                "an unlocked version reports no {header}"
            );
        }
    }
}
