# Installation

Record Store ships as container images published to the GitHub Container
Registry, and as source you can build yourself.

## Prerequisites

| Method | Requirements |
| --- | --- |
| Published images | Docker, and a GitHub token with `read:packages` |
| Docker Compose | Docker with the Compose plugin |
| From source | Rust 1.97.1 (pinned by `rust-toolchain.toml`), a C toolchain |
| Web console | Node.js 24, in addition to one of the above |

A system `protoc` is **not** required. The build vendors what it needs.

## Published images

The shortest path, and the one to use in production. Nothing is compiled.

```bash
echo "$GITHUB_TOKEN" | docker login ghcr.io -u <your-github-username> --password-stdin

docker pull ghcr.io/openelementslabs/record-store:0.1.1
docker pull ghcr.io/openelementslabs/record-store-console:0.1.1
```

Both images cover `linux/amd64` and `linux/arm64`; one pull resolves the right
architecture. Record Store has no built-in credentials, so it will not start
until you supply them:

```bash
docker run --read-only \
  -e RECORD_STORE_ROOT_ACCESS_KEY \
  -e RECORD_STORE_ROOT_SECRET_KEY \
  -e RECORD_STORE_CREDENTIAL_MASTER_KEY \
  -e RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN \
  -p 7600:7600 -p 7601:7601 \
  -v record-store-data:/var/lib/record-store \
  ghcr.io/openelementslabs/record-store:0.1.1
```

To run the server and the console together from the published images:

```bash
git clone https://github.com/OpenElementsLabs/record-store.git
cd record-store
docker compose --env-file .env -f deploy/docker/compose.ghcr.yml up -d
```

The login requirement is not incidental: the repository is private, so its
packages are too. See [Container Images](../deployment/container-images.md) for
tags, digest pinning, and package visibility, and
[Verifying a Release](../deployment/verifying-releases.md) for checking where an
image came from.

## Docker Compose from source

The repository ships three Compose files under `deploy/docker/`. All but the first
build the image from source on first use.

| File | What it runs |
| --- | --- |
| `compose.ghcr.yml` | Record Store and the console, from the published images. |
| `compose.yml` | Record Store alone. S3 on 7600, management on 7601. |
| `compose.console.yml` | Record Store plus the web console on 7602. |

```bash
git clone https://github.com/OpenElementsLabs/record-store.git
cd record-store
docker compose -f deploy/docker/compose.console.yml up --build -d
```

The three source-building files carry development defaults for every secret. They
are marked `change-me` and must not be used anywhere real. `compose.ghcr.yml`
deliberately carries none: it refuses to start until every secret is set. See
[Docker Compose](../deployment/docker-compose.md).

## Building the image yourself

Building is for development and for changes you have not released. Deployments
should use the published images above.

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
data in the `/var/lib/record-store` volume. See
[Persistent Storage](../deployment/persistent-storage.md).

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
