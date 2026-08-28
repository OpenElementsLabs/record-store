use std::collections::{BTreeMap, BTreeSet};

use axum::{
    extract::{Extension, Path, RawQuery, State},
    http::StatusCode,
    response::Response,
};
use percent_encoding::percent_decode_str;
use record_store_core::{ObjectVersionRecord, PartNumber, UploadId, VersionId};
use record_store_service::{
    ServiceListMultipartUploadsRequest, ServiceListRequest, ServiceListVersionsRequest,
};

use crate::error::{S3Error, S3ErrorKind, service_error};
use crate::handlers::bucket::{get_bucket_cors, get_bucket_versioning};
use crate::response::{
    bucket_name, decode_continuation_token, encode_continuation_token, object_key, xml_response,
};
use crate::sigv4::S3RequestId;
use crate::xml::{
    CommonPrefix, DeleteMarkerEntry, ListBucketResult, ListMultipartUploadsResult, ListPartsResult,
    ListVersionsResult, ListedPart, ListedUpload, ObjectEntry, VersionEntry,
};
use crate::*;

pub(crate) async fn list_parts(
    state: S3State,
    bucket: String,
    key: String,
    upload_id: &str,
    query: &BTreeMap<String, String>,
    request_id: S3RequestId,
) -> Result<Response, S3Error> {
    let upload_id = upload_id
        .parse::<UploadId>()
        .map_err(|_| S3Error::new(S3ErrorKind::NoSuchUpload, request_id.clone(), &key))?;
    let marker = query
        .get("part-number-marker")
        .map(|value| value.parse::<PartNumber>())
        .transpose()
        .map_err(|_| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), &key))?;
    let maximum = query
        .get("max-parts")
        .map_or(Ok(1_000), |value| value.parse::<usize>())
        .map_err(|_| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), &key))?
        .min(1_000);
    let bucket_name = bucket_name(&bucket, &request_id)?;
    let object_key = object_key(&key, &request_id, &key)?;
    let parts = state
        .services
        .objects
        .list_parts(&bucket_name, &object_key, upload_id, marker, maximum + 1)
        .await
        .map_err(|error| service_error(error, request_id.clone(), &key))?;
    let truncated = parts.len() > maximum;
    let visible = parts.into_iter().take(maximum).collect::<Vec<_>>();
    let next_marker = truncated
        .then(|| visible.last().map(|part| part.number.get()))
        .flatten();
    xml_response(
        StatusCode::OK,
        &ListPartsResult {
            xmlns: "http://s3.amazonaws.com/doc/2006-03-01/",
            bucket,
            key,
            upload_id: upload_id.to_string(),
            part_number_marker: marker.map_or(0, PartNumber::get),
            next_part_number_marker: next_marker,
            max_parts: maximum,
            is_truncated: truncated,
            parts: visible
                .into_iter()
                .map(|part| ListedPart {
                    part_number: part.number.get(),
                    last_modified: part.modified_at.to_rfc3339(),
                    etag: format!("\"{}\"", part.etag),
                    size: part.size,
                })
                .collect(),
        },
        request_id,
        "/",
    )
}

pub(crate) async fn list_multipart_uploads(
    state: S3State,
    bucket: String,
    raw_query: Option<String>,
    request_id: S3RequestId,
) -> Result<Response, S3Error> {
    let query = query_map(raw_query.as_deref())
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &bucket))?;
    let maximum = query
        .get("max-uploads")
        .map_or(Ok(1_000), |value| value.parse::<usize>())
        .map_err(|_| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), &bucket))?
        .min(1_000);
    let marker = query
        .get("upload-id-marker")
        .map(|value| value.parse::<UploadId>())
        .transpose()
        .map_err(|_| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), &bucket))?;
    let prefix = query.get("prefix").cloned().unwrap_or_default();
    let result = state
        .services
        .objects
        .list_multipart_uploads(ServiceListMultipartUploadsRequest {
            bucket: bucket_name(&bucket, &request_id)?,
            prefix: prefix.clone(),
            upload_id_marker: marker,
            maximum_uploads: maximum,
        })
        .await
        .map_err(|error| service_error(error, request_id.clone(), &bucket))?;
    let is_truncated = result.next_upload_id_marker.is_some();
    xml_response(
        StatusCode::OK,
        &ListMultipartUploadsResult {
            xmlns: "http://s3.amazonaws.com/doc/2006-03-01/",
            bucket,
            prefix,
            upload_id_marker: marker.map(|value| value.to_string()),
            next_upload_id_marker: result.next_upload_id_marker.map(|value| value.to_string()),
            max_uploads: maximum,
            is_truncated,
            uploads: result
                .uploads
                .into_iter()
                .map(|upload| ListedUpload {
                    key: upload.key.to_string(),
                    upload_id: upload.id.to_string(),
                    initiated: upload.initiated_at.to_rfc3339(),
                })
                .collect(),
        },
        request_id,
        "/",
    )
}

pub(crate) async fn list_object_versions(
    state: S3State,
    bucket: String,
    raw_query: Option<String>,
    request_id: S3RequestId,
) -> Result<Response, S3Error> {
    let query = query_map(raw_query.as_deref())
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &bucket))?;
    let maximum = query
        .get("max-keys")
        .map_or(Ok(1_000), |value| value.parse::<usize>())
        .map_err(|_| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), &bucket))?
        .min(1_000);
    let key_marker = query.get("key-marker").cloned();
    let version_marker = query
        .get("version-id-marker")
        .map(|value| value.parse::<VersionId>())
        .transpose()
        .map_err(|_| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), &bucket))?;
    let prefix = query.get("prefix").cloned().unwrap_or_default();
    let result = state
        .services
        .objects
        .list_versions(ServiceListVersionsRequest {
            bucket: bucket_name(&bucket, &request_id)?,
            prefix: prefix.clone(),
            key_marker: key_marker.clone(),
            version_id_marker: version_marker,
            maximum_keys: maximum,
        })
        .await
        .map_err(|error| service_error(error, request_id.clone(), &bucket))?;
    let mut versions = Vec::new();
    let mut markers = Vec::new();
    for listed in result.versions {
        match listed.record {
            ObjectVersionRecord::Object { metadata, is_null } => versions.push(VersionEntry {
                key: metadata.key.to_string(),
                version_id: if is_null {
                    "null".into()
                } else {
                    metadata.version_id.to_string()
                },
                is_latest: listed.is_latest,
                last_modified: metadata.modified_at.to_rfc3339(),
                etag: format!("\"{}\"", metadata.etag),
                size: metadata.size,
                storage_class: "STANDARD",
            }),
            ObjectVersionRecord::DeleteMarker { marker, is_null } => {
                markers.push(DeleteMarkerEntry {
                    key: marker.key.to_string(),
                    version_id: if is_null {
                        "null".into()
                    } else {
                        marker.version_id.to_string()
                    },
                    is_latest: listed.is_latest,
                    last_modified: marker.created_at.to_rfc3339(),
                })
            }
        }
    }
    let is_truncated = result.next_key_marker.is_some();
    xml_response(
        StatusCode::OK,
        &ListVersionsResult {
            xmlns: "http://s3.amazonaws.com/doc/2006-03-01/",
            name: bucket,
            prefix,
            key_marker,
            version_id_marker: version_marker.map(|value| value.to_string()),
            next_key_marker: result.next_key_marker,
            next_version_id_marker: result.next_version_id_marker.map(|value| value.to_string()),
            max_keys: maximum,
            is_truncated,
            versions,
            delete_markers: markers,
        },
        request_id,
        "/",
    )
}

pub(crate) fn has_query_flag(query: Option<&str>, expected: &str) -> bool {
    query.unwrap_or_default().split('&').any(|item| {
        let name = item.split_once('=').map_or(item, |(name, _)| name);
        decode_query_component(name).is_ok_and(|name| name == expected)
    })
}

pub(crate) fn query_map(query: Option<&str>) -> Result<BTreeMap<String, String>, S3ErrorKind> {
    let mut values = BTreeMap::new();
    for item in query
        .unwrap_or_default()
        .split('&')
        .filter(|item| !item.is_empty())
    {
        let (name, value) = item.split_once('=').unwrap_or((item, ""));
        let name = decode_query_component(name)?;
        let value = decode_query_component(value)?;
        if values.insert(name, value).is_some() {
            return Err(S3ErrorKind::InvalidRequest);
        }
    }
    Ok(values)
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum RequestedVersion {
    Null,
    Id(VersionId),
}

pub(crate) fn requested_version(
    query: &BTreeMap<String, String>,
) -> Result<Option<RequestedVersion>, S3ErrorKind> {
    query
        .get("versionId")
        .map(|value| {
            if value == "null" {
                Ok(RequestedVersion::Null)
            } else {
                value
                    .parse::<VersionId>()
                    .map(RequestedVersion::Id)
                    .map_err(|_| S3ErrorKind::InvalidRequest)
            }
        })
        .transpose()
}

#[derive(Debug, Default)]
pub(crate) struct ListQuery {
    list_type: Option<u8>,
    prefix: String,
    delimiter: Option<String>,
    max_keys: Option<usize>,
    continuation_token: Option<String>,
    start_after: Option<String>,
}

pub(crate) async fn list_objects_v2(
    State(state): State<S3State>,
    Path(bucket): Path<String>,
    RawQuery(raw_query): RawQuery,
    Extension(request_id): Extension<S3RequestId>,
) -> Result<Response, S3Error> {
    if has_query_flag(raw_query.as_deref(), "cors") {
        return get_bucket_cors(state, bucket, request_id).await;
    }
    if has_query_flag(raw_query.as_deref(), "versioning") {
        return get_bucket_versioning(state, bucket, request_id).await;
    }
    if has_query_flag(raw_query.as_deref(), "versions") {
        return list_object_versions(state, bucket, raw_query, request_id).await;
    }
    if has_query_flag(raw_query.as_deref(), "uploads") {
        return list_multipart_uploads(state, bucket, raw_query, request_id).await;
    }
    let query = parse_list_query(raw_query.as_deref().unwrap_or_default())
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &format!("/{bucket}")))?;
    if query.list_type != Some(2) {
        return Err(S3Error::new(
            S3ErrorKind::NotImplemented,
            request_id,
            &format!("/{bucket}"),
        ));
    }
    let name = bucket_name(&bucket, &request_id)?;
    let continuation_start = query
        .continuation_token
        .as_deref()
        .map(decode_continuation_token)
        .transpose()
        .map_err(|kind| S3Error::new(kind, request_id.clone(), &bucket))?;
    let start_after = continuation_start.or_else(|| query.start_after.clone());
    let delimiter = query.delimiter.filter(|value| !value.is_empty());
    let result = state
        .services
        .objects
        .list(ServiceListRequest {
            bucket: name,
            prefix: query.prefix.clone(),
            delimiter: delimiter.clone(),
            maximum_keys: query.max_keys.unwrap_or(1_000),
            start_after,
        })
        .await
        .map_err(|error| service_error(error, request_id.clone(), &bucket))?;
    let document = ListBucketResult {
        xmlns: "http://s3.amazonaws.com/doc/2006-03-01/",
        name: bucket,
        prefix: query.prefix,
        delimiter,
        key_count: result.objects.len() + result.common_prefixes.len(),
        max_keys: query.max_keys.unwrap_or(1_000),
        is_truncated: result.is_truncated,
        continuation_token: query.continuation_token,
        start_after: query.start_after,
        next_continuation_token: result.next_marker.as_deref().map(encode_continuation_token),
        contents: result
            .objects
            .into_iter()
            .map(|metadata| ObjectEntry {
                key: metadata.key.to_string(),
                last_modified: metadata.modified_at.to_rfc3339(),
                etag: format!("\"{}\"", metadata.etag),
                size: metadata.size,
                storage_class: "STANDARD",
            })
            .collect(),
        common_prefixes: result
            .common_prefixes
            .into_iter()
            .map(|prefix| CommonPrefix { prefix })
            .collect(),
    };
    xml_response(
        StatusCode::OK,
        &document,
        request_id,
        &format!("/{}", document.name),
    )
}

pub(crate) fn parse_list_query(query: &str) -> Result<ListQuery, S3ErrorKind> {
    let mut parsed = ListQuery::default();
    let mut seen = BTreeSet::new();
    for item in query.split('&') {
        if item.is_empty() {
            continue;
        }
        let (raw_name, raw_value) = item.split_once('=').unwrap_or((item, ""));
        let name = decode_query_component(raw_name)?;
        let value = decode_query_component(raw_value)?;
        match name.as_str() {
            "list-type" | "prefix" | "delimiter" | "max-keys" | "continuation-token"
            | "start-after" => {
                if !seen.insert(name.clone()) {
                    return Err(S3ErrorKind::InvalidRequest);
                }
            }
            _ => continue,
        }
        match name.as_str() {
            "list-type" => {
                parsed.list_type = Some(value.parse().map_err(|_| S3ErrorKind::InvalidRequest)?);
            }
            "prefix" => parsed.prefix = value,
            "delimiter" => parsed.delimiter = Some(value),
            "max-keys" => {
                parsed.max_keys = Some(value.parse().map_err(|_| S3ErrorKind::InvalidRequest)?);
            }
            "continuation-token" => parsed.continuation_token = Some(value),
            "start-after" => parsed.start_after = Some(value),
            _ => {}
        }
    }
    Ok(parsed)
}

pub(crate) fn decode_query_component(value: &str) -> Result<String, S3ErrorKind> {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return Err(S3ErrorKind::InvalidRequest);
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    String::from_utf8(percent_decode_str(value).collect()).map_err(|_| S3ErrorKind::InvalidRequest)
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use tower::ServiceExt;

    use crate::test_support::*;
    use chrono::Utc;

    #[tokio::test]
    async fn list_objects_v2_continuation_is_bounded_and_lossless() {
        let (_directory, application, _credentials) = test_router().await;
        let now = Utc::now();
        let created = application
            .clone()
            .oneshot(signed_request(
                Method::PUT,
                "/page-bucket",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("create bucket");
        assert_eq!(created.status(), StatusCode::OK);
        for key in ["a", "b", "c"] {
            let response = application
                .clone()
                .oneshot(signed_request(
                    Method::PUT,
                    &format!("/page-bucket/{key}"),
                    key.as_bytes(),
                    &[],
                    TEST_ACCESS_KEY,
                    TEST_SECRET_KEY,
                    now,
                ))
                .await
                .expect("put listing object");
            assert_eq!(response.status(), StatusCode::OK);
        }

        let first = application
            .clone()
            .oneshot(signed_request(
                Method::GET,
                "/page-bucket?list-type=2&max-keys=2",
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("first listing page");
        assert_eq!(first.status(), StatusCode::OK);
        let first = body_text(first).await;
        assert!(first.contains("<Key>a</Key>"));
        assert!(first.contains("<Key>b</Key>"));
        assert!(first.contains("<IsTruncated>true</IsTruncated>"));
        let token = xml_value(&first, "NextContinuationToken").expect("continuation token");

        let second = application
            .oneshot(signed_request(
                Method::GET,
                &format!("/page-bucket?list-type=2&max-keys=2&continuation-token={token}"),
                b"",
                &[],
                TEST_ACCESS_KEY,
                TEST_SECRET_KEY,
                now,
            ))
            .await
            .expect("second listing page");
        assert_eq!(second.status(), StatusCode::OK);
        let second = body_text(second).await;
        assert!(second.contains("<Key>c</Key>"));
        assert!(second.contains("<IsTruncated>false</IsTruncated>"));
    }

    /// ListObjectsV2's delimiter is what makes a flat keyspace look like folders
    /// to a client. Prefixes must be rolled up and not also listed as keys.
    #[tokio::test]
    async fn a_delimiter_rolls_nested_keys_up_into_common_prefixes() {
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;
        for key in ["top.txt", "a/one.txt", "a/two.txt", "b/one.txt"] {
            put(&application, "photos", key, b"x").await;
        }

        let response = send(
            &application,
            Method::GET,
            "/photos?list-type=2&delimiter=/",
            b"",
            &[],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let document = body_text(response).await;
        assert!(document.contains("<Prefix>a/</Prefix>"), "{document}");
        assert!(document.contains("<Prefix>b/</Prefix>"), "{document}");
        assert!(document.contains("<Key>top.txt</Key>"), "{document}");
        assert!(
            !document.contains("<Key>a/one.txt</Key>"),
            "a rolled-up key must not also be listed: {document}"
        );
    }

    #[tokio::test]
    async fn a_prefix_restricts_the_listing_to_its_own_keyspace() {
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;
        for key in ["a/one.txt", "a/two.txt", "b/one.txt"] {
            put(&application, "photos", key, b"x").await;
        }

        let response = send(
            &application,
            Method::GET,
            "/photos?list-type=2&prefix=a/",
            b"",
            &[],
        )
        .await;
        let document = body_text(response).await;
        assert!(document.contains("a/one.txt"), "{document}");
        assert!(!document.contains("b/one.txt"), "{document}");
        assert_eq!(xml_value(&document, "KeyCount"), Some("2"), "{document}");
    }

    /// `start-after` is how a client resumes without a token, so it must exclude
    /// the key it names rather than repeating it.
    #[tokio::test]
    async fn start_after_excludes_the_key_it_names() {
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;
        for key in ["a", "b", "c"] {
            put(&application, "photos", key, b"x").await;
        }

        let response = send(
            &application,
            Method::GET,
            "/photos?list-type=2&start-after=a",
            b"",
            &[],
        )
        .await;
        let document = body_text(response).await;
        assert!(!document.contains("<Key>a</Key>"), "{document}");
        assert!(document.contains("<Key>b</Key>"), "{document}");
    }

    #[tokio::test]
    async fn an_unparseable_listing_query_is_refused() {
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;
        for query in [
            "/photos?list-type=2&max-keys=lots",
            "/photos?list-type=2&continuation-token=not-base64!!",
        ] {
            let response = send(&application, Method::GET, query, b"", &[]).await;
            assert!(
                response.status().is_client_error(),
                "{query} was accepted: {}",
                response.status()
            );
        }
    }

    /// Version listing is how a client sees history, including the delete
    /// markers that hide an object without removing it.
    #[tokio::test]
    async fn version_listing_reports_versions_and_delete_markers() {
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;
        let enable = send(
            &application,
            Method::PUT,
            "/photos?versioning",
            b"<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>",
            &[],
        )
        .await;
        assert_eq!(enable.status(), StatusCode::OK);

        put(&application, "photos", "note.txt", b"one").await;
        put(&application, "photos", "note.txt", b"two").await;
        let deleted = send(&application, Method::DELETE, "/photos/note.txt", b"", &[]).await;
        assert_eq!(deleted.status(), StatusCode::NO_CONTENT);

        let response = send(&application, Method::GET, "/photos?versions", b"", &[]).await;
        assert_eq!(response.status(), StatusCode::OK);
        let document = body_text(response).await;
        assert_eq!(
            document.matches("<Version>").count(),
            2,
            "both writes must remain listed: {document}"
        );
        assert!(
            document.contains("<DeleteMarker>"),
            "the delete must appear as a marker: {document}"
        );
    }

    /// Multipart listings are how a client discovers uploads it can resume or
    /// must abort; an upload missing from either listing leaks storage.
    #[tokio::test]
    async fn in_progress_multipart_uploads_and_their_parts_are_listable() {
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

        let part = send(
            &application,
            Method::PUT,
            &format!("/photos/big.bin?partNumber=1&uploadId={upload_id}"),
            &[b'x'; 16],
            &[],
        )
        .await;
        assert_eq!(part.status(), StatusCode::OK);

        let uploads = send(&application, Method::GET, "/photos?uploads", b"", &[]).await;
        assert_eq!(uploads.status(), StatusCode::OK);
        let document = body_text(uploads).await;
        assert!(document.contains(&upload_id), "{document}");

        let parts = send(
            &application,
            Method::GET,
            &format!("/photos/big.bin?uploadId={upload_id}"),
            b"",
            &[],
        )
        .await;
        assert_eq!(parts.status(), StatusCode::OK);
        let document = body_text(parts).await;
        assert_eq!(xml_value(&document, "PartNumber"), Some("1"), "{document}");
    }

    #[tokio::test]
    async fn listing_parts_of_an_unknown_upload_reports_no_such_upload() {
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;
        let response = send(
            &application,
            Method::GET,
            "/photos/big.bin?uploadId=0195f0c8-0000-7000-8000-0000000000ff",
            b"",
            &[],
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let document = body_text(response).await;
        assert_eq!(
            xml_value(&document, "Code"),
            Some("NoSuchUpload"),
            "{document}"
        );
    }

    #[tokio::test]
    async fn listings_against_an_absent_bucket_report_no_such_bucket() {
        let (_directory, application, _credentials) = test_router().await;
        for uri in ["/absent?list-type=2", "/absent?versions", "/absent?uploads"] {
            let response = send(&application, Method::GET, uri, b"", &[]).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        }
    }
}
