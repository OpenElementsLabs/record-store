# OES

OES is a self-hosted object storage system under active development. This repository currently provides the production-oriented single-node foundation; it does not yet expose S3 object APIs or implement distributed storage.

## Current capabilities

- Streaming object writes and reads behind an asynchronous `ObjectStore` trait, including range reads and SHA-256 calculation during upload.
- A crash-resistant local filesystem backend using create-only temporary files, fsync, atomic rename, immutable UUID-addressed payloads, and logical keys that never become filesystem paths.
- A durable embedded Redb metadata catalog behind a replaceable `MetadataRepository` trait.
- Validated TOML configuration with environment overrides.
- Axum operational endpoints with structured errors, request IDs, readiness probes, request tracing, body limits, and bounded graceful shutdown.
- Typed authentication and authorization contracts without a credential implementation or plaintext-secret storage.
- Versioned Protobuf build infrastructure for future internal node contracts.
- Operational CLI, container image, integration tests, and CI.

Object upload and download operations are currently internal Rust APIs exercised by tests. No public S3-compatible routes are claimed or implemented.

## Architecture

The server is a single process with explicit dependencies:

```text
oes-server -> oes-api -> oes-storage -> oes-metadata
                |             |
                +----------> oes-core
```

`oes-config` and `oes-observability` handle process startup concerns. `oes-auth` owns future identity and policy boundaries. `oes-protocol` compiles versioned internal Protobuf messages without introducing a network service.

Local state uses this layout:

```text
<data-directory>/
├── metadata/catalog.redb
├── objects/<2 hex>/<2 hex>/<object UUID>
├── system/
└── tmp/
```

Metadata maps `(BucketId, ObjectKey)` to immutable payload identifiers. User-controlled object keys are validated and are never concatenated into physical paths. A configured temporary directory should be on the same filesystem as the data directory so publication by rename remains atomic.

## Requirements

- Rust 1.97.1 (installed automatically by `rustup` from `rust-toolchain.toml`)
- No system `protoc` installation is required; the protocol build uses a vendored compiler.

## Build and test

```bash
cargo build --workspace
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Tests create isolated temporary directories and do not write to the configured development data directory.

## Run

Defaults listen on `0.0.0.0:9000` and store state in `./data`:

```bash
cargo run --bin oes-server
```

To use the example file:

```bash
cargo run --bin oes-server -- --config oes.example.toml
```

Operational routes are:

- `GET /health` — process liveness only.
- `GET /ready` — verifies writable storage and a writable metadata transaction.
- `GET /api/v1/system/info` — safe name, version, and readiness status.

The CLI provides:

```bash
cargo run --bin oes -- version
cargo run --bin oes -- server check-config --config oes.example.toml
cargo run --bin oes -- server status --endpoint http://127.0.0.1:9000
```

### Configuration

File values are overlaid on safe defaults, then environment variables take precedence. Unknown TOML fields and invalid values fail startup. Supported overrides are:

| Environment variable | Configuration field |
| --- | --- |
| `OES_SERVER_BIND_ADDRESS` | `server.bind_address` |
| `OES_SERVER_PORT` | `server.port` |
| `OES_SERVER_MAX_REQUEST_SIZE_BYTES` | `server.max_request_size_bytes` |
| `OES_SERVER_SHUTDOWN_GRACE_PERIOD_SECONDS` | `server.shutdown_grace_period_seconds` |
| `OES_STORAGE_DATA_DIRECTORY` | `storage.data_directory` |
| `OES_STORAGE_TEMPORARY_DIRECTORY` | `storage.temporary_directory` |
| `OES_LOG` | `observability.log_filter` |
| `OES_LOG_JSON` | `observability.json` |
| `OES_CONFIG_FILE` | server/CLI configuration file selection |

## Docker

```bash
docker build -f deploy/docker/Dockerfile -t oes .
docker run --read-only -p 9000:9000 -v oes-data:/var/lib/oes oes
```

For local Compose:

```bash
docker compose -f deploy/docker/compose.yml up --build
```

The runtime image uses a non-root user, has an explicit data volume and health check, and works with a read-only root filesystem.

## Repository structure

```text
apps/oes-server       daemon startup and lifecycle
apps/oes-cli          operational command-line client
crates/oes-core       strongly typed domain model
crates/oes-config     configuration loading and validation
crates/oes-storage    streaming storage contract and filesystem backend
crates/oes-metadata   metadata contract and embedded durable catalog
crates/oes-api        HTTP routes, middleware, and graceful serving
crates/oes-auth       authentication and authorization contracts
crates/oes-observability structured tracing initialization
crates/oes-protocol   generated internal protocol types
proto/                versioned Protobuf sources
deploy/docker/        container and Compose definitions
```
