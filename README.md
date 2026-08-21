# OES

OES is a self-hosted, single-node object storage service written in Rust. It provides a deliberately small S3-compatible API on port 7600 and an OES management API on port 7601. It does not claim complete S3 compatibility.

## Supported S3 subset

- AWS Signature Version 4 header authentication
- `ListBuckets`, `CreateBucket`, `HeadBucket`, and empty `DeleteBucket`
- streaming `PutObject`, `GetObject`, `HeadObject`, and idempotent `DeleteObject`
- `ListObjectsV2` with prefix, delimiter, max-keys, start-after, and continuation tokens
- single HTTP byte ranges
- content type, `x-amz-meta-*` metadata, and MD5-compatible single-part ETags

Multipart upload, versioning, ACLs, presigned URLs, server-side copy, and AWS's `aws-chunked` trailing-checksum upload encoding are currently unsupported. Unsupported routes return an S3 XML `NotImplemented` error.

## Architecture and durability

Protocol crates call shared application services; they do not access filesystem internals:

```text
oes-s3 ─────┐
            ├──> oes-service ──> oes-storage ──> oes-metadata
oes-api ────┘          │               │
                       └──────────────> oes-core
```

Payloads are immutable and addressed by generated UUIDs. Logical bucket names and object keys never become filesystem paths. Uploads stream through bounded chunks into create-only temporary files while SHA-256 and MD5 are calculated, then use fsync and atomic rename before metadata publication. A durable publication journal resolves the payload/metadata crash window on startup. Replaced and deleted payloads use a durable cleanup queue.

Local state uses this layout:

```text
<data-directory>/
├── metadata/catalog.redb
├── metadata/credentials.redb
├── objects/<2 hex>/<2 hex>/<object UUID>
├── system/
└── tmp/
```

Keep the temporary directory on the same filesystem as the data directory so publication by rename remains atomic.

## Build and test

Rust 1.97.1 is selected by `rust-toolchain.toml`. A system `protoc` is not required.

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features --locked
cargo build --workspace --release --locked
```

Storage microbenchmarks are reproducible with:

```bash
cargo bench -p oes-storage --bench storage
```

## Run

OES intentionally has no built-in production credentials. Set an access key and a secret of at least 16 characters:

```bash
export OES_ROOT_ACCESS_KEY='local-admin'
export OES_ROOT_SECRET_KEY='replace-with-a-long-random-secret'
cargo run --bin oes -- server
```

The equivalent daemon entry point remains available:

```bash
cargo run --bin oes-server
```

Defaults:

```text
S3 API          http://localhost:7600
Management API  http://localhost:7601
```

To load the example file, with secrets still supplied through the environment:

```bash
cargo run --bin oes -- server --config oes.example.toml
```

### AWS CLI

Configure path-style access and the root or a service-account credential:

```bash
export AWS_ACCESS_KEY_ID="$OES_ROOT_ACCESS_KEY"
export AWS_SECRET_ACCESS_KEY="$OES_ROOT_SECRET_KEY"
export AWS_DEFAULT_REGION=us-east-1
export AWS_EC2_METADATA_DISABLED=true
export AWS_REQUEST_CHECKSUM_CALCULATION=WHEN_REQUIRED

aws --endpoint-url http://localhost:7600 s3api list-buckets
aws --endpoint-url http://localhost:7600 s3api create-bucket --bucket demo
aws --endpoint-url http://localhost:7600 s3 cp ./example.pdf s3://demo/example.pdf
aws --endpoint-url http://localhost:7600 s3 cp s3://demo/example.pdf ./downloaded.pdf
aws --endpoint-url http://localhost:7600 s3 ls s3://demo
aws --endpoint-url http://localhost:7600 s3 rm s3://demo/example.pdf
aws --endpoint-url http://localhost:7600 s3api delete-bucket --bucket demo
```

`AWS_REQUEST_CHECKSUM_CALCULATION=WHEN_REQUIRED` tells recent AWS CLI releases to use the normal signed-payload form supported by this initial subset.

### Management API and CLI

Public operational routes are `GET /health`, `GET /ready`, `GET /api/v1/system/info`, and `GET /metrics`. Storage, bucket, and service-account administration routes under `/api/v1` require HTTP Basic authentication with the configured root credentials.

```bash
cargo run --bin oes -- status
cargo run --bin oes -- bucket list
cargo run --bin oes -- bucket create demo
cargo run --bin oes -- bucket delete demo
cargo run --bin oes -- service-account create my-app
cargo run --bin oes -- service-account list
cargo run --bin oes -- service-account revoke <id>
```

The service-account secret is returned only by the explicit create operation. Stored signing material is encrypted with AES-256-GCM. Set a stable `OES_CREDENTIAL_MASTER_KEY` of at least 32 characters in production; otherwise the encryption key is derived from the root secret, so changing that secret makes existing service-account credentials unreadable.

### Configuration

Configuration file values overlay defaults, then environment variables take precedence. Unknown fields and invalid values fail startup.

| Environment variable | Configuration field |
| --- | --- |
| `OES_S3_BIND` | `server.s3_bind` |
| `OES_API_BIND` | `server.api_bind` |
| `OES_SHUTDOWN_TIMEOUT_SECONDS` | `server.shutdown_grace_period_seconds` |
| `OES_STORAGE_DATA_DIRECTORY` | `storage.data_directory` |
| `OES_STORAGE_TEMPORARY_DIRECTORY` | `storage.temporary_directory` |
| `OES_ROOT_ACCESS_KEY` | `auth.root_access_key` |
| `OES_ROOT_SECRET_KEY` | `auth.root_secret_key` |
| `OES_CREDENTIAL_MASTER_KEY` | `auth.credential_master_key` |
| `OES_MAX_CONCURRENT_OPERATIONS` | `limits.maximum_concurrent_operations` |
| `OES_MAX_HEADER_BYTES` | `limits.maximum_header_bytes` |
| `OES_LOG` | `observability.log_filter` |
| `OES_LOG_JSON` | `observability.json` |
| `OES_CONFIG_FILE` | server/CLI configuration file selection |

## Docker

```bash
docker build -f deploy/docker/Dockerfile -t oes .
docker run --read-only \
  -e OES_ROOT_ACCESS_KEY \
  -e OES_ROOT_SECRET_KEY \
  -p 7600:7600 -p 7601:7601 \
  -v oes-data:/var/lib/oes oes
```

For Compose, export the two required root variables first, then run:

```bash
docker compose -f deploy/docker/compose.yml up --build -d
```

The runtime image is non-root, supports a read-only root filesystem, mounts OES state explicitly, exposes only 7600 and 7601, and uses SIGTERM-aware graceful shutdown.

## Repository structure

```text
apps/oes-server       daemon startup and dual-listener lifecycle
apps/oes-cli          server and management command-line interface
crates/oes-core       validated domain model
crates/oes-service    shared bucket/object application services
crates/oes-s3         S3 protocol, SigV4, and XML responses
crates/oes-api        native management HTTP API
crates/oes-storage    streaming storage contract and filesystem backend
crates/oes-metadata   durable indexed embedded catalog
crates/oes-auth       root and encrypted service-account credentials
crates/oes-config     configuration loading and validation
crates/oes-observability structured tracing initialization
crates/oes-protocol   generated future internal protocol types
deploy/docker/        container and Compose definitions
```
