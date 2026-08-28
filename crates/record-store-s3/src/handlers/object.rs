use std::io;

use axum::{
    body::{Body, to_bytes},
    extract::{Extension, Path, RawQuery, State},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{self, HeaderName},
    },
    response::{IntoResponse, Response},
};
use futures_util::TryStreamExt;
use percent_encoding::percent_decode_str;
use record_store_core::{Checksum, CompletedPart, ETag, PartNumber, UploadId, VersionId};
use record_store_service::{
    CopyMetadataDirective, ServiceCompleteMultipartRequest, ServiceCopyRequest,
    ServiceCreateMultipartRequest, ServicePutRequest, ServiceUploadPartRequest,
};
use record_store_storage::upload_stream;
use sha2::{Digest, Sha256};

use crate::error::{S3Error, S3ErrorKind, service_error};
use crate::handlers::listing::{RequestedVersion, list_parts, query_map, requested_version};
use crate::response::{
    ConditionalOutcome, apply_object_headers, bucket_name, conditional_streaming_response,
    custom_metadata, evaluate_conditions, insert_etag, insert_version_id, object_key, parse_range,
    reject_subresources, unsupported_put_headers, xml_response,
};
use crate::sigv4::{PayloadHash, S3RequestId, request_checksum};
use crate::xml::CompleteMultipartUploadDocument;
use crate::xml::{CompleteMultipartUploadResult, CopyObjectResult, InitiateMultipartUploadResult};
use crate::*;

pub(crate) async fn put_object(
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
    let expected_checksum = request_checksum(&headers, &payload_hash)
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &format!("/{bucket}/{key}")))?;
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
        let stream = body.into_data_stream().map_err(io::Error::other);
        let part = state
            .services
            .objects
            .upload_part(ServiceUploadPartRequest {
                bucket: bucket_name,
                key: object_key,
                upload_id,
                number,
                expected_checksum: expected_checksum.clone(),
                body: upload_stream(stream),
            })
            .await
            .map_err(|error| {
                service_error(error, request_id.clone(), &format!("/{bucket}/{key}"))
            })?;
        let mut response = StatusCode::OK.into_response();
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
    let stream = body.into_data_stream().map_err(io::Error::other);
    let result = state
        .services
        .objects
        .put(ServicePutRequest {
            bucket: bucket_name,
            key: object_key,
            content_type,
            custom_metadata,
            expected_checksum,
            body: upload_stream(stream),
        })
        .await
        .map_err(|error| service_error(error, request_id.clone(), &format!("/{bucket}/{key}")))?;
    let mut response = StatusCode::OK.into_response();
    insert_etag(&mut response, &result.metadata);
    insert_version_id(&mut response, result.metadata.version_id);
    Ok(response)
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
    conditional_streaming_response(result, &headers, request_id, &format!("/{bucket}/{key}"))
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
    Ok(response)
}

pub(crate) async fn delete_object(
    State(state): State<S3State>,
    Path((bucket, key)): Path<(String, String)>,
    RawQuery(raw_query): RawQuery,
    Extension(request_id): Extension<S3RequestId>,
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
    let mut response = StatusCode::NO_CONTENT.into_response();
    match version {
        Some(RequestedVersion::Id(version_id)) => {
            state
                .services
                .objects
                .delete_version(&bucket_name, object_key, version_id)
                .await
                .map_err(|error| service_error(error, request_id, &format!("/{bucket}/{key}")))?;
            insert_version_id(&mut response, version_id);
        }
        Some(RequestedVersion::Null) => {
            state
                .services
                .objects
                .delete_null_version(&bucket_name, object_key)
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
    let bytes = to_bytes(body, 1024 * 1024)
        .await
        .map_err(|_| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), &key))?;
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
            "/photos/a.txt?legal-hold",
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
}
