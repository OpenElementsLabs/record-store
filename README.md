# OES

OES is a self-hosted object storage service written in Rust. It supports a simple standalone mode and a single-region replicated cluster mode. Public S3 traffic uses port 7600, the native management API uses 7601, the web console uses 7602, and authenticated internal gRPC uses 7603. Every listener is configurable; OES does not use ports 9000 or 9001.

## Supported S3 surface

- AWS Signature Version 4 header authentication and presigned GET/PUT URLs
- `ListBuckets`, `CreateBucket`, `HeadBucket`, and empty `DeleteBucket`
- streaming `PutObject`, `GetObject`, `HeadObject`, and idempotent `DeleteObject`
- `ListObjectsV2` with bounded pagination, prefix, delimiter, and continuation tokens
- multipart create, streamed part upload, persisted part listing, completion, abort, and upload listing
- bucket versioning (`Disabled`, `Enabled`, and `Suspended`), immutable version reads/deletes, delete markers, and `ListObjectVersions`
- per-bucket CORS configuration, unsigned browser preflights, and CORS headers on matching S3 responses
- streaming same-bucket and cross-bucket `CopyObject` with `COPY` and `REPLACE` metadata directives
- bounded, open-ended, and suffix byte ranges
- `If-Match`, `If-None-Match`, `If-Modified-Since`, and `If-Unmodified-Since`
- content type, `x-amz-meta-*`, SHA-256 checksum validation, single-part ETags, and multipart ETags

Presigned multipart part uploads use the same canonical SigV4 verifier. ACLs, Object Lock enforcement, `UploadPartCopy`, server-side encryption headers, and AWS's `aws-chunked` trailing-checksum encoding are not implemented. Unsupported operations or semantic headers return S3 XML `NotImplemented`; they are never silently accepted.

## Architecture and durability

Protocol crates call shared application services; they do not access filesystem internals. In cluster mode the same service layer is backed by replicated object storage and a strongly consistent metadata repository:

```text
oes-s3 ─────┐                         ┌── local replica store
            ├──> oes-service ────────>├── bounded gRPC replica streams
oes-api ────┘          │              └── checksum verification
                       ▼
             replicated metadata adapter
                       │
                       ▼
              OpenRaft metadata group
              (object bytes never enter Raft)
```

Each node persists an opaque `NodeId`, cluster binding, Raft member ID, and unique node credential separately from its hostname or address. Nodes join with expiring single-use tokens. Internal calls carry node identity, cluster identity, protocol major/minor, software version, storage format, cluster format, trace context, and the node credential. TLS and mutual TLS are supported for production internal networks.

Replica placement is deterministic, capacity-aware, storage-class-aware, and failure-domain-aware. A replicated PUT fans out one bounded stream to selected nodes, each destination independently verifies and publishes its bytes durably, and metadata plus placement become visible atomically only after the acknowledgement policy succeeds. Reads prefer a healthy local replica, fall back before response bytes are emitted, and record missing or corrupt replicas for repair. Leader-elected workers detect failures, repair under-replication, reconcile returning nodes, retain deletion tombstones, rebalance, drain, and decommission without deleting a source before its replacement is committed and verified.

Cluster mode does not by itself make a client endpoint highly available. Use at least three metadata voters and three failure-domain-separated storage nodes, and put healthy S3 ingress nodes behind a load balancer. Two Raft voters cannot survive either member's loss. Erasure coding and multi-region conflict resolution are intentionally outside this release.

Payloads are immutable and addressed by generated UUIDs. Logical bucket names and object keys never become filesystem paths. Uploads stream through bounded chunks into create-only temporary files while SHA-256 and MD5 are calculated, then use fsync and atomic rename before metadata publication.

Optional encryption at rest uses a random per-object or per-part data key, chunked AES-256-GCM authenticated encryption, and a master-key-wrapped data key. The payload header persists the algorithm/format version, nonces, logical size, object binding, and a non-secret key reference. Reads and byte ranges remain streaming and authenticate every accessed chunk. Enable it with `OES_STORAGE_ENCRYPTION_ENABLED=true`; the stable `OES_CREDENTIAL_MASTER_KEY` is then mandatory. Existing plaintext objects remain readable when encryption is first enabled, while all new object and multipart payloads are encrypted. Once an encrypted-store marker exists, startup refuses a missing, mismatched, or disabled key configuration rather than making data unreadable silently.

A durable publication journal resolves the payload/metadata crash window on startup. Replaced and deleted payloads use a durable cleanup queue. Multipart completion has durable completing state and startup reconciliation. Metadata schema version 4 uses ordered, non-destructive migrations.

Local state uses this layout:

```text
<data-directory>/
├── node-identity.json                     # cluster mode
├── node-credential.json                   # cluster mode, permission 0600 on Unix
├── metadata/catalog.redb                  # standalone mode
├── metadata/consensus/consensus-log.redb  # cluster mode
├── metadata/consensus/consensus-state.redb
├── metadata/consensus/snapshots/
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

Dependency security is checked with `tests/rust-audit.sh`. The 2026-08-22
review upgraded `quick-xml` to 0.41.0 for RUSTSEC-2026-0194 and
RUSTSEC-2026-0195. One unscored RustSec advisory, RUSTSEC-2026-0235, is
narrowly excepted: `rkyv` 0.7.46 appears in `Cargo.lock` only as an inactive
optional serialization backend of `rust_decimal` through
`openraft -> byte-unit`. OES does not compile or process rkyv archives. The
audit script first proves that `cargo tree -e features -i rkyv@0.7.46` is empty
and fails if it becomes reachable; all other advisories remain fatal. Remove
the exception when the upstream dependency chain moves to rkyv 0.8.17 or
removes the optional backend.

Storage microbenchmarks are reproducible with `cargo bench -p oes-storage --bench storage`.

Real-client compatibility checks exercise boto3, AWS SDK for JavaScript v3, and AWS SDK for Go against an ephemeral encrypted OES data directory on the fixed listeners. They cover bucket/object I/O, listing, multipart completion, presigned requests, browser CORS, ranges, versioning, historical reads, and copy behavior:

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
S3 API          http://localhost:7600 (also serves /e/<token> embeds)
Management API  http://localhost:7601
Web console     http://localhost:7602 (also serves /s/<token> share pages)
Internal RPC    0.0.0.0:7603 (cluster mode only; do not publish publicly)
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
aws --endpoint-url http://localhost:7600 s3api put-bucket-cors --bucket demo \
  --cors-configuration '{"CORSRules":[{"AllowedOrigins":["https://app.example.com"],"AllowedMethods":["PUT","GET","HEAD"],"AllowedHeaders":["content-type","x-amz-*"],"ExposeHeaders":["ETag","x-amz-version-id"],"MaxAgeSeconds":3600}]}'
aws --endpoint-url http://localhost:7600 s3 cp ./example.pdf s3://demo/example.pdf
aws --endpoint-url http://localhost:7600 s3 cp s3://demo/example.pdf ./downloaded.pdf
aws --endpoint-url http://localhost:7600 s3api list-object-versions --bucket demo
```

Set `OES_ROOT_S3_ENABLED=false` after service-account policies are established to keep root credentials off the application data plane.

Browser access is denied by default. Configure CORS on each bucket that a web
origin may reach; OES does not apply a deployment-wide wildcard. A successful
preflight is unauthenticated but grants only the origins, methods, and request
headers stored on that bucket. The following signed request still needs its
ordinary S3 permission or valid presigned URL. OES never emits
`Access-Control-Allow-Credentials` because S3 browser authorization belongs in
the signature rather than ambient cookies.

### Management API and CLI

Only `GET /health` and `GET /ready` are public. System information is part of
the authenticated management plane, and `GET /metrics` accepts only the
dedicated `OES_METRICS_SCRAPE_TOKEN`. Set `OES_MANAGEMENT_TOKEN` in the CLI
environment to the configured system, storage, or auditor token.

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

Cluster administration uses separate management authorization. System administrators can mutate cluster state; storage administrators and auditors can read cluster status, while ordinary S3 service accounts receive no cluster permission.

```bash
export OES_MANAGEMENT_TOKEN="$OES_MANAGEMENT_SYSTEM_TOKEN"
cargo run --bin oes -- cluster init
cargo run --bin oes -- cluster status
cargo run --bin oes -- cluster issue-join-token --lifetime-seconds 3600
cargo run --bin oes -- node list
cargo run --bin oes -- node inspect <node-id>
cargo run --bin oes -- node drain <node-id>
cargo run --bin oes -- node maintenance <node-id>
cargo run --bin oes -- node resume <node-id>
cargo run --bin oes -- node decommission <node-id>
cargo run --bin oes -- repair status
cargo run --bin oes -- rebalance start
cargo run --bin oes -- rebalance status
```

The first process configured with `OES_MODE=cluster` and no seeds forms the initial one-member group. Join another storage node with a token from the command above:

```bash
cargo run --bin oes -- node join \
  --control storage-1.internal:7603 \
  --token "$OES_CLUSTER_JOIN_TOKEN" \
  --config ./node-2.toml
```

The joining node persists the returned binding and unique credential before activation. A restart reuses both. A node refuses a conflicting cluster or Raft identity rather than silently rebinding stale data.

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
| `OES_MODE` | `server.mode` (`standalone`, `cluster`, or `control`) |
| `OES_RPC_BIND` | `server.rpc_bind` |
| `OES_RPC_ADVERTISE` | `server.rpc_advertise` |
| `OES_CLUSTER_SEEDS` | `cluster.seeds` (comma-separated) |
| `OES_CLUSTER_JOIN_TOKEN` | one-time cluster admission token |
| `OES_CLUSTER_STORAGE_CLASS` | `cluster.storage_class` |
| `OES_CLUSTER_FAILURE_DOMAIN` | `cluster.failure_domain` |
| `OES_CLUSTER_REPLICATION_FACTOR` | initial replicated policy |
| `OES_CLUSTER_MOVEMENT_CONCURRENCY` | node-local movement concurrency |
| `OES_CLUSTER_MOVEMENT_BYTES_PER_SECOND` | movement bandwidth ceiling |
| `OES_CLUSTER_RECONCILE_INTERVAL_SECONDS` | returning-node reconciliation interval |
| `OES_CLUSTER_CAPACITY_LOW_WATERMARK_PERCENT` | initial cluster low-capacity watermark |
| `OES_CLUSTER_CAPACITY_HIGH_WATERMARK_PERCENT` | initial cluster high-capacity watermark |
| `OES_CLUSTER_CAPACITY_CRITICAL_WATERMARK_PERCENT` | initial cluster critical-capacity watermark |
| `OES_CLUSTER_TLS_CERTIFICATE` | internal TLS certificate chain |
| `OES_CLUSTER_TLS_PRIVATE_KEY` | internal TLS private key |
| `OES_CLUSTER_TLS_PEER_CA` | internal peer trust root |
| `OES_CLUSTER_TLS_CLIENT_CA` | internal client trust root (enables mTLS) |
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
| `OES_METRICS_SCRAPE_TOKEN` | `auth.metrics_scrape_token` |
| `OES_MAX_CONCURRENT_OPERATIONS` | `limits.maximum_concurrent_operations` |
| `OES_MAX_HEADER_BYTES` | `limits.maximum_header_bytes` |
| `OES_WEBHOOK_ALLOW_HTTP` | `webhooks.allow_http` |
| `OES_WEBHOOK_ALLOW_PRIVATE_NETWORKS` | `webhooks.allow_private_networks` |
| `OES_WEBHOOK_TIMEOUT_SECONDS` | `webhooks.request_timeout_seconds` |
| `OES_WEBHOOK_MAXIMUM_ATTEMPTS` | `webhooks.maximum_attempts` |
| `OES_WEBHOOK_POLL_INTERVAL_SECONDS` | `webhooks.poll_interval_seconds` |
| `OES_LIFECYCLE_INTERVAL_SECONDS` | `lifecycle.interval_seconds` |
| `OES_LIFECYCLE_BATCH_SIZE` | `lifecycle.batch_size` |
| `OES_SHARING_SHARES_ENABLED` | `sharing.shares_enabled` |
| `OES_SHARING_EMBEDS_ENABLED` | `sharing.embeds_enabled` |
| `OES_SHARING_MAXIMUM_LIFETIME_DAYS` | `sharing.maximum_lifetime_days` |
| `OES_SHARING_REQUIRE_EXPIRATION` | `sharing.require_expiration` |
| `OES_SHARING_REQUIRE_PASSWORD` | `sharing.require_share_password` |
| `OES_SHARING_MAXIMUM_ACCESS_COUNT` | `sharing.maximum_access_count` |
| `OES_SHARING_PASSWORD_ATTEMPTS_PER_MINUTE` | `sharing.password_attempts_per_minute` |
| `OES_SHARING_TOKEN_PROBES_PER_MINUTE` | `sharing.token_probes_per_minute` |
| `OES_SHARING_UNLOCK_LIFETIME_HOURS` | `sharing.unlock_lifetime_hours` |
| `OES_SHARING_PREVIEW_TEXT_LIMIT_BYTES` | `sharing.preview_text_limit_bytes` |
| `OES_SHARING_SHARE_BASE_URL` | `sharing.share_base_url` |
| `OES_SHARING_EMBED_BASE_URL` | `sharing.embed_base_url` |
| `OES_LOG` | `observability.log_filter` |
| `OES_LOG_JSON` | `observability.json` |
| `OES_CONFIG_FILE` | server/CLI configuration selection |

## Preview, share links, and embeds

Stored objects are usable directly rather than only administrable. The console
previews an object; a *share link* gives a person read access to one object
through an OES page; an *embed link* gives a website or application a read-only
URL for the bytes. All three resolve through the same authoritative object
service, so there is no second copy of anything.

```text
                          OES object
                               │
            ┌──────────────────┼──────────────────┐
            ▼                  ▼                  ▼
         Preview            Share              Embed
      authenticated       a person        a site or an app
      console :7602      /s/<token>          /e/<token>
                        console :7602       S3 API :7600
```

A share link and an embed link are different capabilities, and they are
published in different places. A share is a page OES renders, so it lives on the
console alongside the viewer that shows it. An embed serves object bytes into
somebody else's page, so it lives on the S3-compatible endpoint that already
publishes object bytes — which is what lets a deployment expose storage to the
internet while the management plane and the console stay closed. Set
`sharing.embed_base_url` when storage is published under its own hostname.

Both are capabilities rather than credentials. The opaque token in the path is
the entire authorization; it names one object and one version policy and can
express nothing else. Neither can list, write, delete, or reach any other
object, and neither is ever an S3 credential. Every request re-resolves the
token against durable state, so a revocation takes effect on the next one.

| | Share link | Embed link |
| --- | --- | --- |
| Intended for | A person | A website or application |
| Delivered by | Console `:7602` | S3 API `:7600` |
| Version | Current, or a pinned `VersionId` | Current, or a pinned `VersionId` |
| Access | View, download, or both | Read-only bytes |
| Optional controls | Password, expiry, strict access budget | Origin allowlist, expiry |
| Caching | `no-store`, so revocation is immediate | Short, bounded revalidation |

Only media types OES is prepared to be responsible for are served inline:
JPEG, PNG, WebP, GIF, MP4, WebM, MP3, Ogg, WAV, PDF, plain text, Markdown, CSV,
and JSON. A declared type is corroborated against the object's leading bytes
before anything is rendered, so an upload labelled `image/png` that begins with
`<html>` is refused. HTML, SVG, XML, and script are never rendered inline and
never embeddable inline; they remain downloadable as attachments. Downloads are
unchanged: always `Content-Disposition: attachment`, always `nosniff`, whatever
the object turns out to be.

Capability tokens carry 256 bits of entropy from the operating system's
cryptographic generator. They are stored as a lookup digest plus an
AES-256-GCM-sealed copy under the deployment's master key, so an administrator
can copy a link again without OES holding it in the clear. Share passwords are
stored as salted Argon2 hashes, never a digest, and repeated attempts are
throttled per link and per client so a public link cannot be locked for
everyone. Capability tokens are redacted from request logs and audit records;
audit entries name a share or embed by its stable non-secret identifier instead.

## Web console

The console is an administrative interface for OES. It is a client of the
management API on 7601 and is never required: OES stays fully operable through
the CLI and the API alone.

```text
Applications ──────► S3 API        :7600
Embedding sites ───► S3 API        :7600  /e/<token>
Share recipients ──► Web console   :7602  /s/<token>
Administrators ────► Web console   :7602 ──► Management API :7601
OES internal ──────► Node RPC      :7603
```

The browser talks only to the console's own origin. The console server attaches
the management credential and forwards the request to 7601, so the credential
lives in an HTTP-only cookie the page cannot read, no CORS configuration is
needed, and the browser never reaches storage, metadata, consensus, or 7603.

Public share pages are served by the same application but authorize differently:
that boundary attaches no credential at all, because the token in the path is the
authorization. Embed bytes do not pass through the console.

After sign-in, deployment mode is discovered from `GET /api/v1/system/info`, which reports
`mode` and a capability set. A standalone deployment shows a storage-management
interface with no nodes, quorum, replication, repair, or rebalancing; the same
build exposes those screens when the backend reports cluster mode.

### Develop

Requires Node 24 and a running OES server.

```bash
cd console
npm install
OES_API_URL=http://127.0.0.1:7601 npm run dev   # http://localhost:7602
```

Sign in with a management role token, for example the value of
`OES_MANAGEMENT_SYSTEM_TOKEN`. An auditor token signs in to a read-only console.

### Validate

```bash
cd console
npm run lint
npm run typecheck
npm run test
npm run build
```

End-to-end tests drive a real standalone OES server rather than a mock, so
console and API drift is caught rather than papered over:

```bash
cd console
npm run test:e2e:install   # once, downloads Chromium
npm run test:e2e
```

### Configuration

| Variable | Purpose |
| --- | --- |
| `OES_API_URL` | management API base URL, default `http://127.0.0.1:7601` |
| `OES_CONSOLE_SECURE_COOKIES` | force the session cookie's `Secure` flag; defaults to on in production |
| `PORT` | console listener, default `7602` |

`OES_API_URL` is read on the server at runtime, so one image works in any
deployment and no localhost assumption is compiled into the bundle.

### Object uploads

The browser sends an object as one streaming `PUT`. The `File` handle itself is
the request body, so bytes travel from disk to the network without passing
through the page's heap; object size is not bounded by browser memory.

There is no resume. An interrupted upload fails and has to be sent again from
the first byte, and the console states that rather than implying otherwise.
Resumable browser uploads need multipart operations the management API does not
expose yet: presigned part URLs, so control requests go to 7601 while part
bodies go straight to the S3 API on 7600 and no long-lived secret reaches the
page. The transport is one injected function in
`console/features/objects/upload-transport.ts`, so such a strategy can replace
it without touching the queue, progress, retry, or cancellation UI above it.

## Docker

`OES_MODE` is not set above because standalone is the default; no cluster configuration is required to run one node.

Compose variables may be kept in a repo-root `.env` file (which Git ignores) and loaded explicitly with `--env-file .env`. Use `deploy/docker/compose.console.yml` for one standalone OES node plus the console, or `deploy/docker/compose.yml` for the same node without the console.

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

The default Compose file (`deploy/docker/compose.yml`) runs one standalone node: no cluster configuration, no internal RPC listener, and no control-plane process. It publishes only S3 on localhost:7600 and management on localhost:7601. Development secrets have explicit local defaults and must not be copied into production:

```bash
docker compose -f deploy/docker/compose.yml up --build -d
docker compose -f deploy/docker/compose.yml ps
```

A separate Compose file (`deploy/docker/compose.cluster.yml`) is an explicit, opt-in example of a three-node replicated cluster, a management-only `control` process, and the web console. It publishes S3 on 7600, management on 7601, and the console on 7602; 7603 remains private to the Compose network. Nothing here is required for the default standalone experience:

```bash
docker compose -f deploy/docker/compose.cluster.yml up --build -d
docker compose -f deploy/docker/compose.cluster.yml ps
# open http://localhost:7602 and sign in with OES_MANAGEMENT_SYSTEM_TOKEN
OES_MANAGEMENT_TOKEN=local-development-management-token-change-me \
  docker compose -f deploy/docker/compose.cluster.yml exec control oes cluster status
```

A third Compose file (`deploy/docker/compose.console.yml`) runs a standalone node together with the web console. It publishes S3 on 7600, management on 7601, and the console on 7602:

```bash
docker compose --env-file .env -f deploy/docker/compose.console.yml up --build -d
# open http://localhost:7602 and sign in with OES_MANAGEMENT_SYSTEM_TOKEN
```

The Compose network uses plaintext internal traffic strictly for local development. Configure the cluster TLS fields, preferably mutual TLS, in every real cluster deployment.

The runtime image is non-root, supports a read-only root filesystem, declares 7600/7601/7603, publishes only ports selected by the operator, uses the management health endpoint, and performs SIGTERM-aware graceful shutdown across HTTP, Raft, RPC, and background workers.

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
crates/oes-cluster    membership, placement, health, credentials, and movement model
crates/oes-consensus  persistent OpenRaft metadata state machine and snapshots
crates/oes-events     durable events and signed webhook delivery
crates/oes-lifecycle  incremental lifecycle expiration worker
crates/oes-config     configuration loading and validation
crates/oes-observability structured tracing initialization
crates/oes-protocol   versioned Protobuf internal contracts
crates/oes-rpc        authenticated Tonic consensus and replica transport
crates/oes-replication distributed reads/writes, repair, rebalance, and operations
console/              web console: Next.js, React, Tailwind, TanStack
deploy/docker/        container and Compose definitions
```
