# Installation

Record Store ships as source you build, either directly with Cargo or inside a
container image built from the repository.

!!! note "There are no prebuilt binaries or published images yet"
    Every method below builds from the repository. If you want the shortest path,
    use [Docker Compose](#docker-compose).

## Prerequisites

| Method | Requirements |
| --- | --- |
| Docker Compose | Docker with the Compose plugin |
| Docker | Docker |
| From source | Rust 1.97.1 (pinned by `rust-toolchain.toml`), a C toolchain |
| Web console | Node.js 24, in addition to one of the above |

A system `protoc` is **not** required. The build vendors what it needs.

## Docker Compose

The repository ships three Compose files under `deploy/docker/`. Each builds the
image from source on first use.

| File | What it runs |
| --- | --- |
| `compose.yml` | One standalone node. S3 on 7600, management on 7601. |
| `compose.console.yml` | One standalone node plus the web console on 7602. |
| `compose.cluster.yml` | Three storage nodes, a control node, and the console. |

```bash
git clone https://github.com/OpenElementsLabs/record-store.git
cd record-store
docker compose -f deploy/docker/compose.console.yml up --build -d
```

The Compose files carry development defaults for every secret. They are marked
`change-me` and must not be used anywhere real. See
[Docker Compose](../deployment/docker-compose.md).

## Docker

Build the image and run it directly:

```bash
docker build -f deploy/docker/Dockerfile -t record-store .
```

```bash
docker run --read-only \
  -e RECORD_STORE_ROOT_ACCESS_KEY \
  -e RECORD_STORE_ROOT_SECRET_KEY \
  -e RECORD_STORE_CREDENTIAL_MASTER_KEY \
  -e RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN \
  -e RECORD_STORE_STORAGE_ENCRYPTION_ENABLED=true \
  -p 7600:7600 -p 7601:7601 \
  -v record-store-data:/var/lib/record-store \
  record-store
```

The image runs as a non-root user, supports a read-only root filesystem, and stores
data in the `/var/lib/record-store` volume. See [Docker](../deployment/docker.md).

## From source

```bash
git clone https://github.com/OpenElementsLabs/record-store.git
cd record-store
cargo build --release
```

This produces two binaries in `target/release/`:

| Binary | Purpose |
| --- | --- |
| `record-store` | Operational CLI. Also starts the server with `record-store server`. |
| `record-store-server` | The server daemon on its own. Takes only `--config`. |

Record Store has no built-in credentials, so it will not start until you supply them:

```bash
export RECORD_STORE_ROOT_ACCESS_KEY='trial-access-key'
export RECORD_STORE_ROOT_SECRET_KEY='<a long random secret>'
export RECORD_STORE_CREDENTIAL_MASTER_KEY='<a stable 32+ character master key>'
export RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN='<a distinct 32+ character token>'
./target/release/record-store server
```

## The web console

The console is a separate Next.js application. It is optional: Record Store is fully
operable through the CLI and the management API.

```bash
cd console
npm install
RECORD_STORE_API_URL=http://127.0.0.1:7601 npm run dev
```

The console then listens on <http://localhost:7602>. See
[Web Console](../guides/web-console.md).

## Verify the installation

```bash
curl http://127.0.0.1:7601/health
```

```json
{"status":"ok"}
```

## Next

- [Quick Start](quick-start.md) — create a bucket and store an object
