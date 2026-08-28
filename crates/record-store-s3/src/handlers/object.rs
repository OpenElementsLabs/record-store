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
