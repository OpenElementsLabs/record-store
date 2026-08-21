# OES

OES is a self-hosted, single-node object storage service written in Rust. It exposes an S3-compatible API on port 7600 and a native management API on port 7601. It deliberately does not introduce distributed metadata, replication, or erasure coding.

## Supported S3 surface

- AWS Signature Version 4 header authentication and presigned GET/PUT URLs
- `ListBuckets`, `CreateBucket`, `HeadBucket`, and empty `DeleteBucket`
- streaming `PutObject`, `GetObject`, `HeadObject`, and idempotent `DeleteObject`
- `ListObjectsV2` with bounded pagination, prefix, delimiter, and continuation tokens
- multipart create, streamed part upload, persisted part listing, completion, abort, and upload listing
- bucket versioning (`Disabled`, `Enabled`, and `Suspended`), immutable version reads/deletes, delete markers, and `ListObjectVersions`
- streaming same-bucket and cross-bucket `CopyObject` with `COPY` and `REPLACE` metadata directives
- bounded, open-ended, and suffix byte ranges
- `If-Match`, `If-None-Match`, `If-Modified-Since`, and `If-Unmodified-Since`
- content type, `x-amz-meta-*`, SHA-256 checksum validation, single-part ETags, and multipart ETags

Presigned multipart part uploads use the same canonical SigV4 verifier. ACLs, Object Lock enforcement, `UploadPartCopy`, server-side encryption headers, and AWS's `aws-chunked` trailing-checksum encoding are not implemented. Unsupported operations or semantic headers return S3 XML `NotImplemented`; they are never silently accepted.

## Architecture and durability

Protocol crates call shared application services; they do not access filesystem internals:

```text
oes-s3 ─────┐
            ├──> oes-service ──> oes-storage ──> oes-metadata
oes-api ────┘       │      │
                    │      └──> oes-events ──> signed webhook worker
                    └─────────> oes-core
oes-auth ──> policy engine       oes-audit ──> durable audit catalog
oes-lifecycle ──> incremental metadata scans and audited expiration
```

Payloads are immutable and addressed by generated UUIDs. Logical bucket names and object keys never become filesystem paths. Uploads stream through bounded chunks into create-only temporary files while SHA-256 and MD5 are calculated, then use fsync and atomic rename before metadata publication.

Optional encryption at rest uses a random per-object or per-part data key, chunked AES-256-GCM authenticated encryption, and a master-key-wrapped data key. The payload header persists the algorithm/format version, nonces, logical size, object binding, and a non-secret key reference. Reads and byte ranges remain streaming and authenticate every accessed chunk. Enable it with `OES_STORAGE_ENCRYPTION_ENABLED=true`; the stable `OES_CREDENTIAL_MASTER_KEY` is then mandatory. Existing plaintext objects remain readable when encryption is first enabled, while all new object and multipart payloads are encrypted. Once an encrypted-store marker exists, startup refuses a missing, mismatched, or disabled key configuration rather than making data unreadable silently.

A durable publication journal resolves the payload/metadata crash window on startup. Replaced and deleted payloads use a durable cleanup queue. Multipart completion has durable completing state and startup reconciliation. Metadata schema version 4 uses ordered, non-destructive migrations.

Local state uses this layout:

```text
<data-directory>/
├── metadata/catalog.redb
├── metadata/credentials.redb
├── metadata/audit.redb
├── metadata/events.redb
├── metadata/lifecycle.redb
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

Storage microbenchmarks are reproducible with `cargo bench -p oes-storage --bench storage`.

Real-client compatibility checks exercise boto3, AWS SDK for JavaScript v3, and AWS SDK for Go against an ephemeral encrypted OES data directory on the fixed listeners. They cover bucket/object I/O, listing, multipart completion, presigned requests, ranges, versioning, historical reads, and copy behavior:

```bash
bash tests/compatibility/run.sh
```

The runner installs pinned client dependencies into a temporary directory and removes all test state when it exits.

## Run

OES intentionally has no built-in credentials. Use distinct, stable secrets:

```bash
export OES_ROOT_ACCESS_KEY='local-admin'
export OES_ROOT_SECRET_KEY='replace-with-a-long-random-secret'
export OES_CREDENTIAL_MASTER_KEY='replace-with-a-stable-32-byte-or-longer-master-key'
export OES_MANAGEMENT_SYSTEM_TOKEN='replace-with-a-distinct-32-byte-or-longer-token'
export OES_STORAGE_ENCRYPTION_ENABLED=true
cargo run --bin oes -- server
```

The equivalent daemon entry point is `cargo run --bin oes-server`. Defaults remain:

```text
S3 API          http://localhost:7600
Management API  http://localhost:7601
```

Load the example file with `cargo run --bin oes -- server --config oes.example.toml`; secrets should still come from the environment.

### AWS CLI

Configure path-style access and a root or policy-authorized service-account credential:

```bash
export AWS_ACCESS_KEY_ID="$OES_ROOT_ACCESS_KEY"
export AWS_SECRET_ACCESS_KEY="$OES_ROOT_SECRET_KEY"
export AWS_DEFAULT_REGION=us-east-1
export AWS_EC2_METADATA_DISABLED=true
export AWS_REQUEST_CHECKSUM_CALCULATION=WHEN_REQUIRED

aws --endpoint-url http://localhost:7600 s3api list-buckets
aws --endpoint-url http://localhost:7600 s3api create-bucket --bucket demo
aws --endpoint-url http://localhost:7600 s3api put-bucket-versioning \
  --bucket demo --versioning-configuration Status=Enabled
aws --endpoint-url http://localhost:7600 s3 cp ./example.pdf s3://demo/example.pdf
aws --endpoint-url http://localhost:7600 s3 cp s3://demo/example.pdf ./downloaded.pdf
aws --endpoint-url http://localhost:7600 s3api list-object-versions --bucket demo
```

Set `OES_ROOT_S3_ENABLED=false` after service-account policies are established to keep root credentials off the application data plane.

### Management API and CLI

Public operational routes are `GET /health`, `GET /ready`, `GET /api/v1/system/info`, and `GET /metrics`. Administrative routes under `/api/v1` use dedicated bearer tokens. Set `OES_MANAGEMENT_TOKEN` in the CLI environment to the configured system, storage, or auditor token.

If no system token is configured, legacy root Basic authentication remains available for development compatibility and OES emits a warning. Management roles are separate from S3 policies: system administrators have full access, storage administrators manage storage/buckets/integrity/lifecycle, and auditors have read-only access to audit and operational metadata.

```bash
export OES_MANAGEMENT_TOKEN="$OES_MANAGEMENT_SYSTEM_TOKEN"
cargo run --bin oes -- status
cargo run --bin oes -- bucket list
cargo run --bin oes -- bucket create demo
cargo run --bin oes -- bucket versioning enable demo
cargo run --bin oes -- service-account create my-app
cargo run --bin oes -- credential rotate <account-id>
cargo run --bin oes -- policy create ./policy.json
cargo run --bin oes -- policy attach <policy-id> <account-id>
cargo run --bin oes -- webhook list
cargo run --bin oes -- audit --limit 100
cargo run --bin oes -- verify object demo path/to/object
cargo run --bin oes -- storage inspect
cargo run --bin oes -- storage repair              # dry run
cargo run --bin oes -- storage repair --apply      # explicit orphan deletion
```

Service-account and webhook signing secrets are returned only when created or rotated. Stored signing material is encrypted with AES-256-GCM under the injected `OES_CREDENTIAL_MASTER_KEY`. The same injected master material derives a domain-separated object key-encryption key when payload encryption is enabled. OES refuses to create encrypted credentials without it and refuses startup if encrypted records or payload state exist but the key is unavailable. The master key is never stored by OES.

S3 service accounts use attached allow/deny policies. Explicit deny overrides allow; no matching allow is an implicit deny. Policy resources use canonical decoded logical keys and support only a trailing wildcard, avoiding filesystem or ambiguous wildcard semantics.

### Webhooks and lifecycle

Storage events are persisted separately from audit events. Matching webhook deliveries run outside the object upload response path, use HMAC-SHA256 signatures, persist state across restart, and stop after bounded exponential retries. HTTPS and public network targets are the safe defaults; HTTP and private targets require explicit configuration. Redirects are disabled and attempts have a fixed timeout.

Lifecycle rules support prefix-scoped current-object expiration and non-current-version expiration. The supervised worker scans indexed metadata in bounded pages, persists a cursor per rule, and writes an audit event for each successful deletion.

### Offline metadata backup

Stop OES before backup or restore. The command obtains an exclusive data-directory lock, so it refuses to race a running server. Backups contain versioned, SHA-256-verified metadata database files, not object payloads or configuration secrets.

```bash
cargo run --bin oes -- server backup-metadata ./backup-2026-08-21
cargo run --bin oes -- server restore-metadata ./backup-2026-08-21
```

Restore refuses an incompatible manifest or a non-empty target metadata directory.

### Configuration

Configuration file values overlay defaults, then environment variables take precedence. Unknown fields and invalid values fail startup.

| Environment variable | Configuration field |
| --- | --- |
| `OES_S3_BIND` | `server.s3_bind` |
| `OES_API_BIND` | `server.api_bind` |
| `OES_SHUTDOWN_TIMEOUT_SECONDS` | `server.shutdown_grace_period_seconds` |
| `OES_STORAGE_DATA_DIRECTORY` | `storage.data_directory` |
| `OES_STORAGE_TEMPORARY_DIRECTORY` | `storage.temporary_directory` |
| `OES_STORAGE_ENCRYPTION_ENABLED` | `storage.encryption_enabled` |
| `OES_ROOT_ACCESS_KEY` | `auth.root_access_key` |
| `OES_ROOT_SECRET_KEY` | `auth.root_secret_key` |
| `OES_CREDENTIAL_MASTER_KEY` | `auth.credential_master_key` |
| `OES_ROOT_S3_ENABLED` | `auth.root_s3_enabled` |
| `OES_MANAGEMENT_SYSTEM_TOKEN` | `auth.management_system_token` |
| `OES_MANAGEMENT_STORAGE_TOKEN` | `auth.management_storage_token` |
| `OES_MANAGEMENT_AUDITOR_TOKEN` | `auth.management_auditor_token` |
| `OES_MAX_CONCURRENT_OPERATIONS` | `limits.maximum_concurrent_operations` |
| `OES_MAX_HEADER_BYTES` | `limits.maximum_header_bytes` |
| `OES_WEBHOOK_ALLOW_HTTP` | `webhooks.allow_http` |
| `OES_WEBHOOK_ALLOW_PRIVATE_NETWORKS` | `webhooks.allow_private_networks` |
| `OES_WEBHOOK_TIMEOUT_SECONDS` | `webhooks.request_timeout_seconds` |
| `OES_WEBHOOK_MAXIMUM_ATTEMPTS` | `webhooks.maximum_attempts` |
| `OES_WEBHOOK_POLL_INTERVAL_SECONDS` | `webhooks.poll_interval_seconds` |
| `OES_LIFECYCLE_INTERVAL_SECONDS` | `lifecycle.interval_seconds` |
| `OES_LIFECYCLE_BATCH_SIZE` | `lifecycle.batch_size` |
| `OES_LOG` | `observability.log_filter` |
| `OES_LOG_JSON` | `observability.json` |
| `OES_CONFIG_FILE` | server/CLI configuration selection |

## Docker

```bash
docker build -f deploy/docker/Dockerfile -t oes .
docker run --read-only \
  -e OES_ROOT_ACCESS_KEY \
  -e OES_ROOT_SECRET_KEY \
  -e OES_CREDENTIAL_MASTER_KEY \
  -e OES_MANAGEMENT_SYSTEM_TOKEN \
  -e OES_STORAGE_ENCRYPTION_ENABLED=true \
  -p 7600:7600 -p 7601:7601 \
  -v oes-data:/var/lib/oes oes
```

For Compose, export those four required values and run `docker compose -f deploy/docker/compose.yml up --build -d`.

The runtime image is non-root, supports a read-only root filesystem, exposes only 7600 and 7601, uses the management health endpoint, and performs SIGTERM-aware graceful shutdown across HTTP and background workers.

## Repository structure

```text
apps/oes-server       startup, dual listeners, backup, and worker supervision
apps/oes-cli          server and management command-line interface
crates/oes-core       validated domain model
crates/oes-service    shared bucket/object application services
crates/oes-s3         S3 protocol, SigV4, XML, multipart, and versioning
crates/oes-api        native management HTTP API and management roles
crates/oes-storage    streaming filesystem backend and recovery journal
crates/oes-metadata   durable indexed catalog and ordered migrations
crates/oes-auth       encrypted credentials and authorization policies
crates/oes-audit      durable bounded security audit trail
crates/oes-events     durable events and signed webhook delivery
crates/oes-lifecycle  incremental lifecycle expiration worker
crates/oes-config     configuration loading and validation
crates/oes-observability structured tracing initialization
crates/oes-protocol   generated future internal protocol types
deploy/docker/        container and Compose definitions
```
