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
