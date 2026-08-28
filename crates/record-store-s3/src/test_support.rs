//! Shared fixtures for the S3 protocol tests.
//!
//! Signing a request the way a real client does is the only way these tests can
//! exercise the authenticated surface, so the helpers here build genuine SigV4
//! and presigned requests against a throwaway store.

use std::sync::Arc;

use axum::http::header::HeaderName;
use axum::{
    Router,
    body::Body,
    http::{HeaderMap, HeaderValue, Method, Request as HttpRequest, Uri, header},
    response::Response,
};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use record_store_auth::{Authorizer, SigningCredentialProvider};
use record_store_auth::{CredentialManager, SigningSecret};
use record_store_core::OrganizationId;
use record_store_metadata::MetadataRepository;
use record_store_metadata::RedbMetadataRepository;
use record_store_service::{ServiceLimits, Services};
use record_store_storage::LocalFilesystemStore;
use record_store_storage::ObjectStore;
use sha2::{Digest, Sha256};
use tempfile::{TempDir, tempdir};

use crate::sigv4::{aws_encode, calculate_signature, canonical_request};
use crate::{S3State, router};

pub(crate) const TEST_ACCESS_KEY: &str = "root-test-access";
pub(crate) const TEST_SECRET_KEY: &str = "root-test-secret-at-least-sixteen";

pub(crate) async fn test_router() -> (TempDir, Router, Arc<CredentialManager>) {
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

pub(crate) fn signed_request(
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

pub(crate) fn presigned_request(
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

pub(crate) async fn body_text(response: Response) -> String {
    let body = response
        .into_body()
        .collect()
        .await
        .expect("response body")
        .to_bytes();
    String::from_utf8(body.to_vec()).expect("UTF-8 response")
}

pub(crate) fn xml_value<'a>(document: &'a str, element: &str) -> Option<&'a str> {
    let start_tag = format!("<{element}>");
    let end_tag = format!("</{element}>");
    let start = document.find(&start_tag)? + start_tag.len();
    let end = document[start..].find(&end_tag)? + start;
    Some(&document[start..end])
}
