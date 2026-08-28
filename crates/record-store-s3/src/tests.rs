use std::sync::Arc;

use axum::http::Request as HttpRequest;
use http_body_util::BodyExt;
use proptest::prelude::*;
use record_store_auth::{
    Action, Authorizer, CredentialManager, PolicyEffect, PolicyStatement, SigningSecret,
};
use record_store_core::{Checksum, OrganizationId};
use record_store_metadata::{MetadataRepository, RedbMetadataRepository};
use record_store_service::ServiceLimits;
use record_store_storage::{LocalFilesystemStore, ObjectStore};
use tempfile::{TempDir, tempdir};
use tower::ServiceExt;

use axum::{
    Router,
    body::Body,
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    response::Response,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, Duration, Utc};
use sha2::{Digest, Sha256};

use super::*;
use crate::error::S3ErrorKind;
use crate::handlers::listing::parse_list_query;
use crate::response::parse_range;
use crate::sigv4::{
    ParsedAuthorization, PayloadHash, aws_encode, calculate_signature, canonical_query,
    canonical_request, request_checksum,
};

const TEST_ACCESS_KEY: &str = "root-test-access";
const TEST_SECRET_KEY: &str = "root-test-secret-at-least-sixteen";

async fn test_router() -> (TempDir, Router, Arc<CredentialManager>) {
    let directory = tempdir().expect("temporary directory");
    let metadata_impl = Arc::new(
        RedbMetadataRepository::open(directory.path().join("metadata.redb"))
            .await
            .expect("metadata repository"),
    );
    let metadata: Arc<dyn MetadataRepository> = metadata_impl;
    let storage_impl = Arc::new(
        LocalFilesystemStore::open(
            directory.path().join("data"),
            directory.path().join("data/tmp"),
            Arc::clone(&metadata),
        )
        .await
        .expect("filesystem store"),
    );
    let storage: Arc<dyn ObjectStore> = storage_impl;
    let services = Services::new(
        storage,
        metadata,
        OrganizationId::new(),
        ServiceLimits {
            maximum_concurrent_operations: 16,
            maximum_custom_metadata_entries: 8,
            maximum_custom_metadata_bytes: 1_024,
        },
    );
    let credentials = Arc::new(
        CredentialManager::open(
            directory.path().join("credentials.redb"),
            TEST_ACCESS_KEY,
            TEST_SECRET_KEY,
            Some(b"s3-test-master-key-at-least-32-bytes"),
        )
        .await
        .expect("credential manager"),
    );
    let provider: Arc<dyn SigningCredentialProvider> = credentials.clone();
    let authorizer: Arc<dyn Authorizer> = credentials.clone();
    (
        directory,
        router(S3State::new(services, provider).with_authorizer(authorizer)),
        credentials,
    )
}

fn signed_request(
    method: Method,
    uri: &str,
    payload: &[u8],
    extra_headers: &[(&str, &str)],
    access_key: &str,
    secret_key: &str,
    time: DateTime<Utc>,
) -> HttpRequest<Body> {
    let uri: Uri = uri.parse().expect("request URI");
    let payload_hash = hex::encode(Sha256::digest(payload));
    let timestamp = time.format("%Y%m%dT%H%M%SZ").to_string();
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, HeaderValue::from_static("localhost"));
    headers.insert(
        HeaderName::from_static("x-amz-content-sha256"),
        HeaderValue::from_str(&payload_hash).expect("payload hash header"),
    );
    headers.insert(
        HeaderName::from_static("x-amz-date"),
        HeaderValue::from_str(&timestamp).expect("timestamp header"),
    );
    for (name, value) in extra_headers {
        headers.insert(
            HeaderName::from_bytes(name.as_bytes()).expect("header name"),
            HeaderValue::from_str(value).expect("header value"),
        );
    }
    let mut signed_headers = headers
        .keys()
        .map(|name| name.as_str().to_owned())
        .collect::<Vec<_>>();
    signed_headers.sort();
    let canonical = canonical_request(&method, &uri, &headers, &signed_headers, payload_hash)
        .expect("canonical request");
    let date = time.format("%Y%m%d").to_string();
    let scope = format!("{date}/us-east-1/s3/aws4_request");
    let canonical_hash = hex::encode(Sha256::digest(canonical.as_bytes()));
    let string_to_sign = format!("AWS4-HMAC-SHA256\n{timestamp}\n{scope}\n{canonical_hash}");
    let signature = calculate_signature(
        &SigningSecret::new(secret_key),
        &date,
        "us-east-1",
        "s3",
        string_to_sign.as_bytes(),
    )
    .expect("signature");
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!(
            "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={}, Signature={}",
            signed_headers.join(";"),
            hex::encode(signature)
        ))
        .expect("authorization header"),
    );
    let mut request = HttpRequest::builder()
        .method(method)
        .uri(uri)
        .body(Body::from(payload.to_vec()))
        .expect("HTTP request");
    *request.headers_mut() = headers;
    request
}

fn presigned_request(
    method: Method,
    path: &str,
    access_key: &str,
    secret_key: &str,
    time: DateTime<Utc>,
    expires: i64,
) -> HttpRequest<Body> {
    let date = time.format("%Y%m%d").to_string();
    let timestamp = time.format("%Y%m%dT%H%M%SZ").to_string();
    let scope = format!("{date}/us-east-1/s3/aws4_request");
    let credential = format!("{access_key}/{scope}");
    let query = format!(
        "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential={}&X-Amz-Date={timestamp}&X-Amz-Expires={expires}&X-Amz-SignedHeaders=host",
        aws_encode(credential.as_bytes(), true)
    );
    let uri: Uri = format!("{path}?{query}").parse().expect("presigned URI");
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, HeaderValue::from_static("localhost"));
    let canonical = canonical_request(
        &method,
        &uri,
        &headers,
        &["host".into()],
        "UNSIGNED-PAYLOAD".into(),
    )
    .expect("presigned canonical request");
    let canonical_hash = hex::encode(Sha256::digest(canonical.as_bytes()));
    let string_to_sign = format!("AWS4-HMAC-SHA256\n{timestamp}\n{scope}\n{canonical_hash}");
    let signature = calculate_signature(
        &SigningSecret::new(secret_key),
        &date,
        "us-east-1",
        "s3",
        string_to_sign.as_bytes(),
    )
    .expect("presigned signature");
    let uri: Uri = format!("{uri}&X-Amz-Signature={}", hex::encode(signature))
        .parse()
        .expect("signed URI");
    let mut request = HttpRequest::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .expect("HTTP request");
    *request.headers_mut() = headers;
    request
}

async fn body_text(response: Response) -> String {
    let body = response
        .into_body()
        .collect()
        .await
        .expect("response body")
        .to_bytes();
    String::from_utf8(body.to_vec()).expect("UTF-8 response")
}

fn xml_value<'a>(document: &'a str, element: &str) -> Option<&'a str> {
    let start_tag = format!("<{element}>");
    let end_tag = format!("</{element}>");
    let start = document.find(&start_tag)? + start_tag.len();
    let end = document[start..].find(&end_tag)? + start;
    Some(&document[start..end])
}

#[test]
fn canonical_request_matches_aws_documentation_example() {
    let uri: Uri = "https://examplebucket.s3.amazonaws.com/test.txt"
        .parse()
        .expect("URI");
    let mut headers = HeaderMap::new();
    headers.insert(
        "host",
        HeaderValue::from_static("examplebucket.s3.amazonaws.com"),
    );
    headers.insert("range", HeaderValue::from_static("bytes=0-9"));
    headers.insert(
        "x-amz-content-sha256",
        HeaderValue::from_static(EMPTY_PAYLOAD_SHA256),
    );
    headers.insert("x-amz-date", HeaderValue::from_static("20130524T000000Z"));
    let signed = vec![
        "host".into(),
        "range".into(),
        "x-amz-content-sha256".into(),
        "x-amz-date".into(),
    ];
    let canonical = canonical_request(
        &Method::GET,
        &uri,
        &headers,
        &signed,
        EMPTY_PAYLOAD_SHA256.into(),
    )
    .expect("canonical request");
    assert_eq!(
        hex::encode(Sha256::digest(canonical.as_bytes())),
        "7344ae5b7ee6c3e7e6b0fe0640412a37625d1fbfff95c48bbb2dc43964946972"
    );
}

#[test]
fn malformed_authorization_and_ranges_are_rejected() {
    assert!(ParsedAuthorization::parse("Bearer secret").is_err());
    assert!(ParsedAuthorization::parse("AWS4-HMAC-SHA256 Credential=x").is_err());
    assert!(parse_range("bytes=5-2", 10).is_err());
    assert!(parse_range("bytes=0-1,4-5", 10).is_err());
    assert_eq!(parse_range("bytes=-4", 10).expect("suffix").offset(), 6);
}

#[test]
fn canonical_query_sorts_and_encodes_values() {
    assert_eq!(
        canonical_query("z=last&a=hello%20world&a=first"),
        "a=first&a=hello%20world&z=last"
    );
}

#[test]
fn client_sha256_checksum_is_strictly_decoded_and_cross_checked() {
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-amz-checksum-sha256",
        HeaderValue::from_str(&STANDARD.encode([7_u8; 32])).expect("header"),
    );
    assert_eq!(
        request_checksum(&headers, &PayloadHash::Unsigned).expect("checksum"),
        Some(Checksum::sha256([7_u8; 32]))
    );
    assert!(matches!(
        request_checksum(&headers, &PayloadHash::Sha256(Checksum::sha256([8_u8; 32]))),
        Err(S3ErrorKind::BadDigest)
    ));
}

#[tokio::test]
async fn presigned_get_put_are_bounded_to_method_and_expiration() {
    let (_directory, application, _credentials) = test_router().await;
    let now = Utc::now();
    let create = signed_request(
        Method::PUT,
        "/presigned-bucket",
        b"",
        &[],
        TEST_ACCESS_KEY,
        TEST_SECRET_KEY,
        now,
    );
    assert_eq!(
        application
            .clone()
            .oneshot(create)
            .await
            .expect("create bucket")
            .status(),
        StatusCode::OK
    );

    let mut put = presigned_request(
        Method::PUT,
        "/presigned-bucket/object.txt",
        TEST_ACCESS_KEY,
        TEST_SECRET_KEY,
        now,
        60,
    );
    *put.body_mut() = Body::from("presigned payload");
    assert_eq!(
        application
            .clone()
            .oneshot(put)
            .await
            .expect("presigned put")
            .status(),
        StatusCode::OK
    );

    let get = presigned_request(
        Method::GET,
        "/presigned-bucket/object.txt",
        TEST_ACCESS_KEY,
        TEST_SECRET_KEY,
        now,
        60,
    );
    let get_uri = get.uri().clone();
    let response = application
        .clone()
        .oneshot(get)
        .await
        .expect("presigned get");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, "presigned payload");

    let mut delete = HttpRequest::builder()
        .method(Method::DELETE)
        .uri(get_uri)
        .body(Body::empty())
        .expect("method-confusion request");
    delete
        .headers_mut()
        .insert(header::HOST, HeaderValue::from_static("localhost"));
    assert_eq!(
        application
            .clone()
            .oneshot(delete)
            .await
            .expect("method-bound URL")
            .status(),
        StatusCode::FORBIDDEN
    );

    let expired = presigned_request(
        Method::GET,
        "/presigned-bucket/object.txt",
        TEST_ACCESS_KEY,
        TEST_SECRET_KEY,
        now - Duration::seconds(120),
        60,
    );
    assert_eq!(
        application
            .oneshot(expired)
            .await
            .expect("expired URL")
            .status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn bucket_cors_controls_preflights_and_actual_presigned_responses() {
    let (_directory, application, _credentials) = test_router().await;
    let now = Utc::now();
    let create = signed_request(
        Method::PUT,
        "/browser-bucket",
        b"",
        &[],
        TEST_ACCESS_KEY,
        TEST_SECRET_KEY,
        now,
    );
    assert_eq!(
        application
            .clone()
            .oneshot(create)
            .await
            .expect("create bucket")
            .status(),
        StatusCode::OK
    );

    let preflight = || {
        HttpRequest::builder()
            .method(Method::OPTIONS)
            .uri("/browser-bucket/report.txt")
            .header(header::ORIGIN, "https://app.example.com")
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "PUT")
            .header(
                header::ACCESS_CONTROL_REQUEST_HEADERS,
                "content-type, x-amz-checksum-sha256",
            )
            .body(Body::empty())
            .expect("preflight request")
    };
    let denied = application
        .clone()
        .oneshot(preflight())
        .await
        .expect("unconfigured preflight");
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert!(
        !denied
            .headers()
            .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN)
    );

    let cors = br#"<CORSConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
      <CORSRule>
        <ID>browser-upload</ID>
        <AllowedOrigin>https://app.example.com</AllowedOrigin>
        <AllowedMethod>PUT</AllowedMethod>
        <AllowedMethod>GET</AllowedMethod>
        <AllowedHeader>content-type</AllowedHeader>
        <AllowedHeader>x-amz-*</AllowedHeader>
        <ExposeHeader>ETag</ExposeHeader>
        <ExposeHeader>x-amz-version-id</ExposeHeader>
        <MaxAgeSeconds>600</MaxAgeSeconds>
      </CORSRule>
    </CORSConfiguration>"#;
    let configure = signed_request(
        Method::PUT,
        "/browser-bucket?cors",
        cors,
        &[("content-type", XML_CONTENT_TYPE)],
        TEST_ACCESS_KEY,
        TEST_SECRET_KEY,
        now,
    );
    assert_eq!(
        application
            .clone()
            .oneshot(configure)
            .await
            .expect("configure CORS")
            .status(),
        StatusCode::OK
    );

    let get_configuration = signed_request(
        Method::GET,
        "/browser-bucket?cors",
        b"",
        &[],
        TEST_ACCESS_KEY,
        TEST_SECRET_KEY,
        now,
    );
    let configuration = application
        .clone()
        .oneshot(get_configuration)
        .await
        .expect("get CORS configuration");
    assert_eq!(configuration.status(), StatusCode::OK);
    let configuration = body_text(configuration).await;
    assert!(configuration.contains("<AllowedOrigin>https://app.example.com</AllowedOrigin>"));
    assert!(configuration.contains("<AllowedHeader>x-amz-*</AllowedHeader>"));

    let allowed = application
        .clone()
        .oneshot(preflight())
        .await
        .expect("allowed preflight");
    assert_eq!(allowed.status(), StatusCode::OK);
    assert_eq!(
        allowed.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
        Some(&HeaderValue::from_static("https://app.example.com"))
    );
    assert_eq!(
        allowed.headers().get(header::ACCESS_CONTROL_ALLOW_METHODS),
        Some(&HeaderValue::from_static("PUT, GET"))
    );
    assert_eq!(
        allowed.headers().get(header::ACCESS_CONTROL_ALLOW_HEADERS),
        Some(&HeaderValue::from_static(
            "content-type, x-amz-checksum-sha256"
        ))
    );
    assert_eq!(
        allowed.headers().get(header::ACCESS_CONTROL_MAX_AGE),
        Some(&HeaderValue::from_static("600"))
    );
    assert!(
        !allowed
            .headers()
            .contains_key(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
    );

    let mut put = presigned_request(
        Method::PUT,
        "/browser-bucket/report.txt",
        TEST_ACCESS_KEY,
        TEST_SECRET_KEY,
        now,
        60,
    );
    put.headers_mut().insert(
        header::ORIGIN,
        HeaderValue::from_static("https://app.example.com"),
    );
    *put.body_mut() = Body::from("browser payload");
    let uploaded = application
        .clone()
        .oneshot(put)
        .await
        .expect("browser upload");
    assert_eq!(uploaded.status(), StatusCode::OK);
    assert_eq!(
        uploaded.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
        Some(&HeaderValue::from_static("https://app.example.com"))
    );
    assert_eq!(
        uploaded
            .headers()
            .get(header::ACCESS_CONTROL_EXPOSE_HEADERS),
        Some(&HeaderValue::from_static("ETag, x-amz-version-id"))
    );
    assert_eq!(
        uploaded.headers().get(header::VARY),
        Some(&HeaderValue::from_static("Origin"))
    );

    let forbidden_origin = HttpRequest::builder()
        .method(Method::OPTIONS)
        .uri("/browser-bucket/report.txt")
        .header(header::ORIGIN, "https://evil.example")
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "PUT")
        .body(Body::empty())
        .expect("foreign-origin preflight");
    let forbidden = application
        .clone()
        .oneshot(forbidden_origin)
        .await
        .expect("foreign-origin response");
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
    assert!(
        !forbidden
            .headers()
            .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN)
    );

    let remove = signed_request(
        Method::DELETE,
        "/browser-bucket?cors",
        b"",
        &[],
        TEST_ACCESS_KEY,
        TEST_SECRET_KEY,
        now,
    );
    assert_eq!(
        application
            .clone()
            .oneshot(remove)
            .await
            .expect("delete CORS configuration")
            .status(),
        StatusCode::NO_CONTENT
    );
    let missing = application
        .oneshot(signed_request(
            Method::GET,
            "/browser-bucket?cors",
            b"",
            &[],
            TEST_ACCESS_KEY,
            TEST_SECRET_KEY,
            now,
        ))
        .await
        .expect("missing CORS response");
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        xml_value(&body_text(missing).await, "Code"),
        Some("NoSuchCORSConfiguration")
    );
}

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

#[tokio::test]
async fn authentication_and_parser_failures_return_s3_xml_without_reaching_storage() {
    let (_directory, application, credentials) = test_router().await;
    let now = Utc::now();

    let unknown = application
        .clone()
        .oneshot(signed_request(
            Method::GET,
            "/",
            b"",
            &[],
            "unknown-access",
            TEST_SECRET_KEY,
            now,
        ))
        .await
        .expect("unknown credential response");
    assert_eq!(unknown.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        xml_value(&body_text(unknown).await, "Code"),
        Some("InvalidAccessKeyId")
    );

    let mut invalid_signature = signed_request(
        Method::GET,
        "/",
        b"",
        &[],
        TEST_ACCESS_KEY,
        TEST_SECRET_KEY,
        now,
    );
    let authorization = invalid_signature
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .expect("authorization text");
    let replacement = if authorization.ends_with('0') {
        '1'
    } else {
        '0'
    };
    let invalid_authorization =
        format!("{}{replacement}", &authorization[..authorization.len() - 1]);
    invalid_signature.headers_mut().insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&invalid_authorization).expect("invalid signature header"),
    );
    let invalid_signature = application
        .clone()
        .oneshot(invalid_signature)
        .await
        .expect("invalid signature response");
    assert_eq!(invalid_signature.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        xml_value(&body_text(invalid_signature).await, "Code"),
        Some("SignatureDoesNotMatch")
    );

    let expired = application
        .clone()
        .oneshot(signed_request(
            Method::GET,
            "/",
            b"",
            &[],
            TEST_ACCESS_KEY,
            TEST_SECRET_KEY,
            now - Duration::hours(1),
        ))
        .await
        .expect("expired timestamp response");
    assert_eq!(expired.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        xml_value(&body_text(expired).await, "Code"),
        Some("RequestTimeTooSkewed")
    );

    let malformed_query = application
        .clone()
        .oneshot(signed_request(
            Method::GET,
            "/missing-bucket?list-type=2&max-keys=invalid",
            b"",
            &[],
            TEST_ACCESS_KEY,
            TEST_SECRET_KEY,
            now,
        ))
        .await
        .expect("malformed query response");
    assert_eq!(malformed_query.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        xml_value(&body_text(malformed_query).await, "Code"),
        Some("InvalidRequest")
    );

    let traversal = application
        .clone()
        .oneshot(signed_request(
            Method::PUT,
            "/missing-bucket/%2E%2E%2Fescape",
            b"payload",
            &[],
            TEST_ACCESS_KEY,
            TEST_SECRET_KEY,
            now,
        ))
        .await
        .expect("traversal response");
    assert!(!traversal.status().is_success());

    let issued = credentials
        .create_service_account("s3-test-client", OrganizationId::new())
        .await
        .expect("issue service account");
    let policy = credentials
        .create_policy(
            "s3-test-access",
            "test-only full access",
            vec![PolicyStatement {
                effect: PolicyEffect::Allow,
                actions: vec![
                    Action::ListBucket,
                    Action::GetObject,
                    Action::PutObject,
                    Action::DeleteObject,
                    Action::GetObjectVersion,
                    Action::DeleteObjectVersion,
                    Action::ManageBucket,
                ],
                resources: vec!["bucket:*".into()],
            }],
        )
        .await
        .expect("create policy");
    credentials
        .attach_policy(issued.info.account.id, policy.id)
        .await
        .expect("attach policy");
    let secret = std::str::from_utf8(issued.secret.expose()).expect("secret text");
    let service_account_request = application
        .clone()
        .oneshot(signed_request(
            Method::GET,
            "/",
            b"",
            &[],
            &issued.info.credential.key_id,
            secret,
            now,
        ))
        .await
        .expect("service account response");
    assert_eq!(service_account_request.status(), StatusCode::OK);

    credentials
        .revoke_service_account(issued.info.account.id)
        .await
        .expect("revoke service account");
    let revoked = application
        .oneshot(signed_request(
            Method::GET,
            "/",
            b"",
            &[],
            &issued.info.credential.key_id,
            secret,
            now,
        ))
        .await
        .expect("revoked credential response");
    assert_eq!(revoked.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        xml_value(&body_text(revoked).await, "Code"),
        Some("AccessDenied")
    );
}

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

#[test]
fn compatibility_registry_is_unique_and_tracks_explicit_gaps() {
    let names = S3_CAPABILITIES
        .iter()
        .map(|capability| capability.name)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(names.len(), S3_CAPABILITIES.len());
    assert!(S3_CAPABILITIES.iter().any(|capability| {
        capability.name == "MultipartUpload" && capability.status == CapabilityStatus::Implemented
    }));
    assert!(S3_CAPABILITIES.iter().any(|capability| {
        capability.name == "UploadPartCopy" && capability.status == CapabilityStatus::Unsupported
    }));
}

proptest! {
    #[test]
    fn risky_protocol_parsers_never_panic(
        authorization in any::<String>(),
        range in any::<String>(),
        size in any::<u64>(),
        query in any::<String>(),
    ) {
        let _ = ParsedAuthorization::parse(&authorization);
        let _ = parse_range(&range, size);
        let _ = parse_list_query(&query);
        let canonical = canonical_query(&query);
        prop_assert_eq!(canonical_query(&canonical), canonical);
    }
}
