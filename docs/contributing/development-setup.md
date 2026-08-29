# Development Setup

## Requirements

| | Version |
| --- | --- |
| Rust | 1.97.1 — pinned by `rust-toolchain.toml` |
| Node.js | 24, for the console |
| Go | 1.24, for compatibility tests |
| Python | 3, for compatibility tests |
| Protobuf | Vendored — nothing to install |

`rustup` reads `rust-toolchain.toml` and installs the right toolchain, with `clippy`
and `rustfmt`, on first build.

## Building

```bash
git clone https://github.com/OpenElementsLabs/record-store.git
cd record-store
cargo build --workspace
```

Two binaries come out:

| | |
| --- | --- |
| `record-store` | The CLI, including `record-store server` |
| `record-store-server` | The daemon alone; accepts only `--config` |

## Running locally

The server refuses to start without credentials, so set them:

```bash
export RECORD_STORE_ROOT_ACCESS_KEY=dev-access-key
export RECORD_STORE_ROOT_SECRET_KEY=dev-secret-key-at-least-16-chars
export RECORD_STORE_CREDENTIAL_MASTER_KEY=dev-master-key-at-least-32-characters
export RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN=dev-system-token-at-least-32-characters
export RECORD_STORE_STORAGE_DATA_DIRECTORY=./data

cargo run --bin record-store -- server
```

Then:

```bash
cargo run --bin record-store -- status --endpoint http://127.0.0.1:7601
```

!!! warning "The master key is fixed for the life of a data directory"
    Changing `RECORD_STORE_CREDENTIAL_MASTER_KEY` after a data directory exists makes
    its sealed contents unreadable. If a local deployment starts failing to decrypt
    after you changed the key, delete `./data` and start over.

An `.env` file plus `set -a; source .env; set +a` is a convenient way to keep these
consistent.

## Console

```bash
cd console
npm ci
npm run dev
```

Serves on `http://localhost:7602` and reads `RECORD_STORE_API_URL`, defaulting to
`http://127.0.0.1:7601`.

```bash
RECORD_STORE_API_URL=http://127.0.0.1:7601 npm run dev
```

## Docker

```bash
docker build -t record-store:dev -f deploy/docker/Dockerfile .

cd deploy/docker
docker compose -f compose.console.yml up -d
```

The development Compose files carry placeholder credentials so `up` works with no
setup. Override every one before using them for anything real.

For a local cluster:

```bash
docker compose -f compose.cluster.yml up -d
```

See [Docker Compose](../deployment/docker-compose.md).

## Before committing

```bash
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features --locked
```

Clippy runs with `-D warnings` in CI. A warning is a failure.

Console:

```bash
cd console
npm run format:check
npm run lint
npm run typecheck
npm test
```

## Useful commands

```bash
# One crate's tests
cargo test -p record-store-s3

# One test by name
cargo test -p record-store-api credential

# Show output from passing tests
cargo test -p record-store-core -- --nocapture

# Storage benchmarks
cargo bench -p record-store-storage --bench storage

# Dependency audit, with the documented exception
tests/rust-audit.sh
```

## Debug logging

```bash
RECORD_STORE_LOG=record_store=debug cargo run --bin record-store -- server
```

Narrow it to the crate you are working on:

```bash
RECORD_STORE_LOG=record_store=info,record_store_s3=debug
```

## Documentation

```bash
pip install -r requirements-docs.txt
mkdocs serve
```

Serves on `http://127.0.0.1:8000` with live reload.

```bash
mkdocs build --strict
```

CI builds with `--strict`, so a broken link or an unrecognised configuration key is a
failure.
