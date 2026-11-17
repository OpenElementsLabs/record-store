# Rust

Use the official `aws-sdk-s3` crate.

!!! warning "Do not depend on Record Store's own crates"
    The `record-store-*` crates are internal to the workspace. They are not published,
    carry no API stability guarantee, and are not an application SDK. Talk to
    Record Store over the S3 API like any other client.

```bash
cargo add aws-sdk-s3 aws-config aws-credential-types
cargo add tokio --features full
```

## Client

```rust
use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::config::{Region, RequestChecksumCalculation, ResponseChecksumValidation};
use aws_sdk_s3::{Client, Config};

pub async fn client(endpoint: &str, access_key: &str, secret_key: &str) -> Client {
    let credentials = Credentials::new(access_key, secret_key, None, None, "record-store");

    let config = Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .endpoint_url(endpoint)
        .force_path_style(true)
        .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
        .response_checksum_validation(ResponseChecksumValidation::WhenRequired)
        .credentials_provider(credentials)
        .build();

    Client::from_conf(config)
}
```

To create the client from the environment variables described in
[SDKs](index.md#environment-variables), call `client` as follows:

```rust
let client = client(
    &std::env::var("RECORD_STORE_ENDPOINT")?,
    &std::env::var("RECORD_STORE_ACCESS_KEY")?,
    &std::env::var("RECORD_STORE_SECRET_KEY")?,
)
.await;
```

The following settings are specific to Record Store:

- `endpoint_url`: the URL of the Record Store S3 API, for example
  `https://storage.example.com`. The S3 API listens on port 7600 by default.
- `access_key` and `secret_key`: a service account's S3 access key pair. The
  management token doesn't work with the S3 API.
- `region`: Record Store has no regions, but SigV4 signs a region into every
  request. Any value works if every client uses the same one.
- `force_path_style(true)`: required. Puts the bucket name in the URL path
  instead of the hostname.
- `request_checksum_calculation` and `response_checksum_validation`: required.
  Stop the SDK from sending upload checksums as `aws-chunked` trailers, which
  Record Store rejects with `NotImplemented`.

## Upload

```rust
use aws_sdk_s3::primitives::ByteStream;

client
    .put_object()
    .bucket("uploads")
    .key("invoices/2026/03/inv-1.pdf")
    .content_type("application/pdf")
    .body(ByteStream::from_path("inv-1.pdf").await?)
    .send()
    .await?;
```

`ByteStream::from_path` streams from disk rather than reading the file into memory.

## Download

```rust
let object = client
    .get_object()
    .bucket("uploads")
    .key("invoices/2026/03/inv-1.pdf")
    .send()
    .await?;

let bytes = object.body.collect().await?.into_bytes();
```

For large objects, read the stream incrementally instead of collecting it.

## List

```rust
let mut pages = client
    .list_objects_v2()
    .bucket("uploads")
    .prefix("invoices/2026/")
    .into_paginator()
    .send();

while let Some(page) = pages.next().await {
    for object in page?.contents() {
        println!("{} {}", object.key().unwrap_or_default(), object.size().unwrap_or_default());
    }
}
```

## Delete

```rust
client
    .delete_object()
    .bucket("uploads")
    .key("invoices/2026/03/inv-1.pdf")
    .send()
    .await?;
```

## Presigned URLs

```rust
use aws_sdk_s3::presigning::PresigningConfig;
use std::time::Duration;

let presigned = client
    .put_object()
    .bucket("uploads")
    .key(&key)
    .presigned(PresigningConfig::expires_in(Duration::from_secs(900))?)
    .await?;

let url = presigned.uri();
```

## Ranges

```rust
let object = client
    .get_object()
    .bucket("uploads")
    .key("big.bin")
    .range("bytes=0-1023")
    .send()
    .await?;
```

## Checksums

The [client](#client) sets `request_checksum_calculation` and
`response_checksum_validation` to `WhenRequired`. If you can't change the client
code, set the equivalent environment variables instead:

```bash
export AWS_REQUEST_CHECKSUM_CALCULATION=WHEN_REQUIRED
export AWS_RESPONSE_CHECKSUM_VALIDATION=WHEN_REQUIRED
```
