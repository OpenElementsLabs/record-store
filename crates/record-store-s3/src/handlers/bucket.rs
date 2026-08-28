use axum::{
    body::{Body, to_bytes},
    extract::{Extension, Path, RawQuery, State},
    http::{
        HeaderValue, StatusCode,
        header::{self},
    },
    response::{IntoResponse, Response},
};
use record_store_core::VersioningState;

use crate::error::{S3Error, S3ErrorKind, service_error};
use crate::handlers::listing::has_query_flag;
use crate::response::{bucket_name, reject_subresources, xml_response};
use crate::sigv4::S3RequestId;
use crate::xml::{BucketEntry, Buckets, ListBucketsResult, Owner, VersioningConfigurationResult};
use crate::xml::{
    CorsConfigurationDocument, CorsConfigurationResult, VersioningConfigurationDocument,
};
use crate::*;

pub(crate) async fn list_buckets(
    State(state): State<S3State>,
    RawQuery(raw_query): RawQuery,
    Extension(request_id): Extension<S3RequestId>,
) -> Result<Response, S3Error> {
    reject_subresources(raw_query.as_deref(), &request_id, "/")?;
    let buckets = state
        .services
        .buckets
        .list()
        .await
        .map_err(|error| service_error(error, request_id.clone(), "/"))?;
    let document = ListBucketsResult {
        xmlns: "http://s3.amazonaws.com/doc/2006-03-01/",
        owner: Owner {
            id: "root",
            display_name: "root",
        },
        buckets: Buckets {
            bucket: buckets
                .into_iter()
                .map(|bucket| BucketEntry {
                    name: bucket.name.to_string(),
                    creation_date: bucket.created_at.to_rfc3339(),
                })
                .collect(),
        },
    };
    xml_response(StatusCode::OK, &document, request_id, "/")
}

pub(crate) async fn create_bucket(
    State(state): State<S3State>,
    Path(bucket): Path<String>,
    RawQuery(raw_query): RawQuery,
    Extension(request_id): Extension<S3RequestId>,
    body: Body,
) -> Result<Response, S3Error> {
    if has_query_flag(raw_query.as_deref(), "cors") {
        return put_bucket_cors(state, bucket, request_id, body).await;
    }
    if has_query_flag(raw_query.as_deref(), "versioning") {
        return put_bucket_versioning(state, bucket, request_id, body).await;
    }
    reject_subresources(raw_query.as_deref(), &request_id, &format!("/{bucket}"))?;
    let name = bucket_name(&bucket, &request_id)?;
    state
        .services
        .buckets
        .create(name)
        .await
        .map_err(|error| service_error(error, request_id.clone(), &format!("/{bucket}")))?;
    let mut response = StatusCode::OK.into_response();
    if let Ok(location) = HeaderValue::from_str(&format!("/{bucket}")) {
        response.headers_mut().insert(header::LOCATION, location);
    }
    Ok(response)
}

pub(crate) async fn head_bucket(
    State(state): State<S3State>,
    Path(bucket): Path<String>,
    RawQuery(raw_query): RawQuery,
    Extension(request_id): Extension<S3RequestId>,
) -> Result<StatusCode, S3Error> {
    reject_subresources(raw_query.as_deref(), &request_id, &format!("/{bucket}"))?;
    let name = bucket_name(&bucket, &request_id)?;
    state
        .services
        .buckets
        .head(&name)
        .await
        .map_err(|error| service_error(error, request_id, &format!("/{bucket}")))?;
    Ok(StatusCode::OK)
}

pub(crate) async fn delete_bucket(
    State(state): State<S3State>,
    Path(bucket): Path<String>,
    RawQuery(raw_query): RawQuery,
    Extension(request_id): Extension<S3RequestId>,
) -> Result<StatusCode, S3Error> {
    if has_query_flag(raw_query.as_deref(), "cors") {
        return delete_bucket_cors(state, bucket, request_id).await;
    }
    reject_subresources(raw_query.as_deref(), &request_id, &format!("/{bucket}"))?;
    let name = bucket_name(&bucket, &request_id)?;
    state
        .services
        .buckets
        .delete(&name)
        .await
        .map_err(|error| service_error(error, request_id, &format!("/{bucket}")))?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn put_bucket_versioning(
    state: S3State,
    bucket: String,
    request_id: S3RequestId,
    body: Body,
) -> Result<Response, S3Error> {
    let bytes = to_bytes(body, 16 * 1024)
        .await
        .map_err(|_| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), &bucket))?;
    let document: VersioningConfigurationDocument = quick_xml::de::from_reader(bytes.as_ref())
        .map_err(|_| S3Error::new(S3ErrorKind::MalformedXml, request_id.clone(), &bucket))?;
    let versioning = match document.status.as_deref() {
        Some("Enabled") => VersioningState::Enabled,
        Some("Suspended") => VersioningState::Suspended,
        _ => {
            return Err(S3Error::new(
                S3ErrorKind::InvalidRequest,
                request_id,
                &bucket,
            ));
        }
    };
    let name = bucket_name(&bucket, &request_id)?;
    state
        .services
        .buckets
        .set_versioning(&name, versioning)
        .await
        .map_err(|error| service_error(error, request_id, &bucket))?;
    Ok(StatusCode::OK.into_response())
}

pub(crate) async fn get_bucket_versioning(
    state: S3State,
    bucket: String,
    request_id: S3RequestId,
) -> Result<Response, S3Error> {
    let name = bucket_name(&bucket, &request_id)?;
    let bucket_record = state
        .services
        .buckets
        .head(&name)
        .await
        .map_err(|error| service_error(error, request_id.clone(), &bucket))?;
    let status = match bucket_record.versioning {
        VersioningState::Disabled => None,
        VersioningState::Enabled => Some("Enabled"),
        VersioningState::Suspended => Some("Suspended"),
    };
    xml_response(
        StatusCode::OK,
        &VersioningConfigurationResult {
            xmlns: "http://s3.amazonaws.com/doc/2006-03-01/",
            status,
        },
        request_id,
        &bucket,
    )
}

pub(crate) async fn put_bucket_cors(
    state: S3State,
    bucket: String,
    request_id: S3RequestId,
    body: Body,
) -> Result<Response, S3Error> {
    let bytes = to_bytes(body, 256 * 1024)
        .await
        .map_err(|_| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), &bucket))?;
    let document: CorsConfigurationDocument = quick_xml::de::from_reader(bytes.as_ref())
        .map_err(|_| S3Error::new(S3ErrorKind::MalformedXml, request_id.clone(), &bucket))?;
    let configuration = document
        .try_into()
        .map_err(|_| S3Error::new(S3ErrorKind::InvalidRequest, request_id.clone(), &bucket))?;
    let name = bucket_name(&bucket, &request_id)?;
    state
        .services
        .buckets
        .set_cors(&name, configuration)
        .await
        .map_err(|error| service_error(error, request_id, &bucket))?;
    Ok(StatusCode::OK.into_response())
}

pub(crate) async fn get_bucket_cors(
    state: S3State,
    bucket: String,
    request_id: S3RequestId,
) -> Result<Response, S3Error> {
    let name = bucket_name(&bucket, &request_id)?;
    let bucket_record = state
        .services
        .buckets
        .head(&name)
        .await
        .map_err(|error| service_error(error, request_id.clone(), &bucket))?;
    let configuration = bucket_record.cors.ok_or_else(|| {
        S3Error::new(
            S3ErrorKind::NoSuchCorsConfiguration,
            request_id.clone(),
            &bucket,
        )
    })?;
    xml_response(
        StatusCode::OK,
        &CorsConfigurationResult::from(&configuration),
        request_id,
        &bucket,
    )
}

pub(crate) async fn delete_bucket_cors(
    state: S3State,
    bucket: String,
    request_id: S3RequestId,
) -> Result<StatusCode, S3Error> {
    let name = bucket_name(&bucket, &request_id)?;
    state
        .services
        .buckets
        .delete_cors(&name)
        .await
        .map_err(|error| service_error(error, request_id, &bucket))?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};

    use crate::test_support::*;

    /// The bucket surface is the first thing an S3 client touches. Each verb has
    /// a distinct meaning and a distinct status, and a client branches on them.
    #[tokio::test]
    async fn the_bucket_lifecycle_reports_a_distinct_status_for_each_step() {
        let (_directory, application, _credentials) = test_router().await;

        let missing = send(&application, Method::HEAD, "/absent", b"", &[]).await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);

        make_bucket(&application, "photos").await;
        let present = send(&application, Method::HEAD, "/photos", b"", &[]).await;
        assert_eq!(present.status(), StatusCode::OK);

        let listed = send(&application, Method::GET, "/", b"", &[]).await;
        assert_eq!(listed.status(), StatusCode::OK);
        let document = body_text(listed).await;
        assert_eq!(xml_value(&document, "Name"), Some("photos"), "{document}");

        let removed = send(&application, Method::DELETE, "/photos", b"", &[]).await;
        assert_eq!(removed.status(), StatusCode::NO_CONTENT);
        let gone = send(&application, Method::HEAD, "/photos", b"", &[]).await;
        assert_eq!(gone.status(), StatusCode::NOT_FOUND);
    }

    /// S3 clients rely on these two failures being distinguishable: one means
    /// "pick another name", the other means "empty it first".
    #[tokio::test]
    async fn a_duplicate_bucket_and_a_full_bucket_report_different_errors() {
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;

        let duplicate = send(&application, Method::PUT, "/photos", b"", &[]).await;
        assert_eq!(duplicate.status(), StatusCode::CONFLICT);
        let document = body_text(duplicate).await;
        assert_eq!(
            xml_value(&document, "Code"),
            Some("BucketAlreadyExists"),
            "{document}"
        );

        put(&application, "photos", "a.txt", b"x").await;
        let occupied = send(&application, Method::DELETE, "/photos", b"", &[]).await;
        assert_eq!(occupied.status(), StatusCode::CONFLICT);
        let document = body_text(occupied).await;
        assert_eq!(
            xml_value(&document, "Code"),
            Some("BucketNotEmpty"),
            "{document}"
        );
    }

    #[tokio::test]
    async fn deleting_a_bucket_that_was_never_created_reports_no_such_bucket() {
        let (_directory, application, _credentials) = test_router().await;
        let response = send(&application, Method::DELETE, "/absent", b"", &[]).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let document = body_text(response).await;
        assert_eq!(
            xml_value(&document, "Code"),
            Some("NoSuchBucket"),
            "{document}"
        );
    }

    /// Versioning is read back through the same subresource it is written to,
    /// and an unset bucket must report the absence rather than inventing a state.
    #[tokio::test]
    async fn bucket_versioning_round_trips_through_its_subresource() {
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;

        let initial = send(&application, Method::GET, "/photos?versioning", b"", &[]).await;
        assert_eq!(initial.status(), StatusCode::OK);
        let document = body_text(initial).await;
        assert!(
            xml_value(&document, "Status").is_none(),
            "an unset bucket reports no status: {document}"
        );

        for state in ["Enabled", "Suspended"] {
            let body = format!(
                "<VersioningConfiguration><Status>{state}</Status></VersioningConfiguration>"
            );
            let applied = send(
                &application,
                Method::PUT,
                "/photos?versioning",
                body.as_bytes(),
                &[],
            )
            .await;
            assert_eq!(applied.status(), StatusCode::OK, "set {state}");

            let read = send(&application, Method::GET, "/photos?versioning", b"", &[]).await;
            let document = body_text(read).await;
            assert_eq!(xml_value(&document, "Status"), Some(state), "{document}");
        }
    }

    #[tokio::test]
    async fn a_versioning_document_that_does_not_parse_is_refused() {
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;
        let response = send(
            &application,
            Method::PUT,
            "/photos?versioning",
            b"<not-a-versioning-document/>",
            &[],
        )
        .await;
        assert!(
            response.status().is_client_error(),
            "a malformed document must be refused: {}",
            response.status()
        );
    }

    /// A CORS policy decides which websites may read a bucket's objects, so it
    /// has to survive a round trip exactly and be removable.
    #[tokio::test]
    async fn a_cors_policy_round_trips_and_can_be_deleted() {
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;

        let absent = send(&application, Method::GET, "/photos?cors", b"", &[]).await;
        assert_eq!(absent.status(), StatusCode::NOT_FOUND);

        let policy = b"<CORSConfiguration><CORSRule><AllowedOrigin>https://example.com</AllowedOrigin><AllowedMethod>GET</AllowedMethod><MaxAgeSeconds>600</MaxAgeSeconds></CORSRule></CORSConfiguration>";
        let applied = send(&application, Method::PUT, "/photos?cors", policy, &[]).await;
        assert_eq!(applied.status(), StatusCode::OK);

        let read = send(&application, Method::GET, "/photos?cors", b"", &[]).await;
        assert_eq!(read.status(), StatusCode::OK);
        let document = body_text(read).await;
        assert_eq!(
            xml_value(&document, "AllowedOrigin"),
            Some("https://example.com"),
            "{document}"
        );

        let removed = send(&application, Method::DELETE, "/photos?cors", b"", &[]).await;
        assert_eq!(removed.status(), StatusCode::NO_CONTENT);
        let gone = send(&application, Method::GET, "/photos?cors", b"", &[]).await;
        assert_eq!(gone.status(), StatusCode::NOT_FOUND);
    }

    /// A rule allowing every origin to do anything is exactly the policy that
    /// makes a bucket world-readable by accident, so the limits are enforced.
    #[tokio::test]
    async fn a_cors_policy_that_exceeds_the_configured_limits_is_refused() {
        let (_directory, application, _credentials) = test_router().await;
        make_bucket(&application, "photos").await;

        let rules = (0..200)
            .map(|index| {
                format!(
                    "<CORSRule><AllowedOrigin>https://{index}.example</AllowedOrigin><AllowedMethod>GET</AllowedMethod></CORSRule>"
                )
            })
            .collect::<String>();
        let document = format!("<CORSConfiguration>{rules}</CORSConfiguration>");
        let response = send(
            &application,
            Method::PUT,
            "/photos?cors",
            document.as_bytes(),
            &[],
        )
        .await;
        assert!(
            response.status().is_client_error(),
            "an unbounded rule set must be refused: {}",
            response.status()
        );
    }

    #[tokio::test]
    async fn bucket_subresources_on_an_absent_bucket_report_no_such_bucket() {
        let (_directory, application, _credentials) = test_router().await;
        for uri in ["/absent?versioning", "/absent?cors"] {
            let response = send(&application, Method::GET, uri, b"", &[]).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        }
    }
}
