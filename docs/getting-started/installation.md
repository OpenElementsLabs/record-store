# Installation

Record Store ships as container images, as Linux packages, as a Helm chart, as
standalone binaries, and as source you can build yourself.

| You want to | Use |
| --- | --- |
| Try it out in one command | [Published images](#published-images) |
| Run it as a service on a Linux host | [Linux packages](../deployment/linux-packages.md) |
| Run it on Kubernetes | [The Helm chart](../deployment/kubernetes.md) |
| Run it without a package manager | [Binary archives](#binary-archives) |
| Change it | [From source](#from-source) |

## Prerequisites

| Method | Requirements |
| --- | --- |
| Published images | Docker |
| Docker Compose | Docker with the Compose plugin |
| Linux packages | A distribution with systemd; Debian 11 or RHEL 8 onwards |
| Helm chart | Kubernetes 1.25 or newer, Helm 3 |
| Binary archives | Linux on `amd64` or `arm64` |
| From source | Rust 1.97.1 (pinned by `rust-toolchain.toml`), a C toolchain |
| Web console | Node.js 24, in addition to one of the above |

A system `protoc` is **not** required. The build vendors what it needs.

## Published images

The shortest path, and the one to use in production. Nothing is compiled.

```bash
docker pull ghcr.io/openelementslabs/record-store:latest
docker pull ghcr.io/openelementslabs/record-store-console:latest
```

`latest` is the newest stable release, which is what you want while trying this
out. For anything you intend to keep running, name the release instead, so an
upgrade is something you decide rather than something that happens:

```bash
docker pull ghcr.io/openelementslabs/record-store:0.1.3
docker pull ghcr.io/openelementslabs/record-store-console:0.1.3
```

Either way, keep the two images on the same version. Both cover `linux/amd64`
and `linux/arm64`; one pull resolves the right architecture.

Record Store has no built-in credentials, so it will not start until you supply
them:

```bash
docker run --read-only \
  -e RECORD_STORE_ROOT_ACCESS_KEY \
  -e RECORD_STORE_ROOT_SECRET_KEY \
  -e RECORD_STORE_CREDENTIAL_MASTER_KEY \
  -e RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN \
  -p 7600:7600 -p 7601:7601 \
  -v record-store-data:/var/lib/record-store \
  ghcr.io/openelementslabs/record-store:latest
```

To run the server and the console together from the published images:

```bash
git clone https://github.com/OpenElementsLabs/record-store.git
cd record-store
docker compose --env-file .env -f deploy/docker/compose.ghcr.yml up -d
```

Both packages are public, so no `docker login` is needed. `RECORD_STORE_VERSION`
selects the tag that Compose file uses, and defaults to the release this page
documents:

```bash
RECORD_STORE_VERSION=latest \
  docker compose --env-file .env -f deploy/docker/compose.ghcr.yml up -d
```

See [Container Images](../deployment/container-images.md) for the full tag list,
how to choose between them, and digest pinning, and
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

## Linux packages

A `.deb` and an `.rpm` for `amd64` and `arm64`, installing the server, the CLI, a
systemd service and a configuration file, and generating credentials unique to
the machine.

```bash
sudo apt-get install ./record-store_0.1.3_amd64.deb    # Debian, Ubuntu
sudo dnf install ./record-store-0.1.3-1.x86_64.rpm     # RHEL, Rocky, Fedora
```

The binaries are statically linked, so the packages depend on nothing and
install on Debian 11 and RHEL 8 onwards. Nothing starts until you enable it.
See [Linux Packages](../deployment/linux-packages.md).

## Kubernetes

```bash
helm install record-store \
  oci://ghcr.io/openelementslabs/charts/record-store --version 0.1.3 \
  --namespace record-store --create-namespace \
  --set auth.rootAccessKey=admin \
  --set auth.rootSecretKey="$(openssl rand -hex 24)" \
  --set auth.credentialMasterKey="$(openssl rand -hex 32)" \
  --set auth.managementSystemToken="$(openssl rand -hex 32)"
```

See [Kubernetes](../deployment/kubernetes.md) for storage, ingress, availability
and where credentials really belong.

## Binary archives

For running Record Store without a package manager or a container. Each release
attaches two archives per architecture:

| Archive | Use it when |
| --- | --- |
| `record-store-0.1.3-linux-amd64-musl.tar.gz` | Anywhere. Statically linked, no libc dependency. |
| `record-store-0.1.3-linux-amd64.tar.gz` | You want exactly what is inside the container image. |

```bash
tar xzf record-store-0.1.3-linux-amd64-musl.tar.gz
sudo install -m 0755 record-store record-store-server /usr/local/bin/
record-store --version
```

Both contain `record-store` and `record-store-server`. Check them against
`SHA256SUMS` from the same release first — see
[Verifying a Release](../deployment/verifying-releases.md).

Nothing is installed around them: no service, no configuration, no account. For
a managed service on a Linux host, use the packages above.

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

## Check the machine before starting

```bash
record-store server --config /etc/record-store/config.toml doctor
```

This reports everything that would stop the deployment working — an unwritable data
directory, an occupied port, a temporary directory on the wrong filesystem, a missing
encryption key — without starting anything and without printing any secret. It exits
0 when nothing failed and 7 when something did.

Every failure names its corrective action:

```text
FAIL  atomic_publication            /mnt/fast/tmp and /var/lib/record-store/objects are on different filesystems
                                    -> put storage.temporary_directory on the same filesystem as the data
                                       directory; a payload cannot be published atomically across a mount boundary
```

## Verify the installation

```bash
curl http://127.0.0.1:7601/health
```

```json
{"status":"ok"}
```

See [Health and Readiness](../operations/health-and-readiness.md) for what `/health`,
`/ready`, and `doctor` each answer, and why they are three different questions.

## Next

- [Quick Start](quick-start.md) — create a bucket and store an object
