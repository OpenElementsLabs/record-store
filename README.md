<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/logo-lockup-dark.svg">
    <img src="docs/assets/logo-lockup.svg" alt="Record Store" width="75%">
  </picture>
</p>

<p align="center">
  Self-hosted, S3-compatible storage for records that need version history, retention controls, and verifiable integrity.
</p>

<p align="center">
  <a href="https://github.com/OpenElementsLabs/record-store/actions/workflows/ci.yml?query=branch%3Amain"><img src="https://github.com/OpenElementsLabs/record-store/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI"></a>
  <a href="https://github.com/OpenElementsLabs/record-store/actions/workflows/docs.yml?query=branch%3Amain"><img src="https://github.com/OpenElementsLabs/record-store/actions/workflows/docs.yml/badge.svg?branch=main" alt="Documentation"></a>
  <a href="https://scorecard.dev/viewer/?uri=github.com/OpenElementsLabs/record-store"><img src="https://api.scorecard.dev/projects/github.com/OpenElementsLabs/record-store/badge" alt="OpenSSF Scorecard"></a>
  <a href="https://github.com/OpenElementsLabs/record-store/releases/latest"><img src="https://img.shields.io/github/v/release/OpenElementsLabs/record-store?sort=semver&display_name=tag&label=release&color=195477" alt="Latest release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/OpenElementsLabs/record-store?label=license&color=195477" alt="Apache-2.0 license"></a>
</p>

# Overview
[Record Store](https://record-store.io) is a self-hosted, S3-compatible storage
service for files that need version history, retention controls, and verifiable
integrity. Upload and retrieve files with S3 clients, manage them through a CLI or
web console, and give people or applications read access through share and embed
links.

**Deployment today: one machine, one copy of your data, no external database.**
Replication and erasure coding are not implemented. Durability depends on the
underlying storage and your backups.

[Product page](https://record-store.io) ·
[Documentation](https://openelementslabs.github.io/record-store/) ·
[Installation](https://openelementslabs.github.io/record-store/getting-started/installation/) ·
[Changelog](CHANGELOG.md)

- [Is Record Store right for you?](#is-record-store-right-for-you)
- [Quickstart](#quickstart)
- [Install with Docker](#install-with-docker)
- [S3 compatibility](#s3-compatibility)
- [Share and embed links](#share-and-embed-links)
- [Architecture](#architecture)
- [Operations and documentation](#operations-and-documentation)
- [Development](#development)
- [License](#license)

## Is Record Store right for you?

| If you need… | What Record Store offers today |
| --- | --- |
| Self-hosted storage for records | Immutable payloads, optional bucket versioning, and retention controls |
| Evidence of file integrity | Checksums on write and read, plus signed proof bundles for offline verification |
| Access through S3 tools | Common S3 operations; [some features are unsupported](#s3-compatibility) |
| File sharing | Revocable share pages and read-only embed URLs |
| Encryption at rest | Optional AES-256-GCM payload encryption; you must preserve the deployment's master key |
| Access control | Allow/deny policies for S3 service accounts and separate management roles |
| Event notifications | Signed webhooks for storage events |
| Automatic expiration | Lifecycle rules for current and non-current object versions |
| A simple deployment | A single-machine server with embedded metadata databases and an optional web console |
| Built-in replication or automatic failover | Not implemented; recovery requires restoring or recovering the machine |
| Tamper-evident audit history | In development; durable audit logging is available today |

## Quickstart

This walkthrough runs the server and console locally from source. For published
container images, see [Install with Docker](#install-with-docker).

### Prerequisites

| Tool | Requirement |
| --- | --- |
| Git | To clone the repository |
| Rust | The version selected in [`rust-toolchain.toml`](rust-toolchain.toml) |
| Node.js and npm | Node.js 24, for the web console |

### 1. Clone the repository

```bash
git clone https://github.com/OpenElementsLabs/record-store.git
cd record-store
```

### 2. Configure credentials and start the server

Replace each placeholder below with your own value. Use distinct secrets and keep
the credential master key stable across restarts.

```bash
export RECORD_STORE_ROOT_ACCESS_KEY='local-admin'
export RECORD_STORE_ROOT_SECRET_KEY='<your-long-random-secret>'
export RECORD_STORE_CREDENTIAL_MASTER_KEY='<your-stable-master-key-at-least-32-bytes>'
export RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN='<your-distinct-token-at-least-32-bytes>'
export RECORD_STORE_STORAGE_ENCRYPTION_ENABLED=true

cargo run --bin record-store -- server
```

Record Store does not store the master key. Back it up securely alongside your
configuration secrets; encrypted data requires it.

### 3. Start the console

In a second terminal, from the repository root:

```bash
cd console
npm install
RECORD_STORE_API_URL=http://127.0.0.1:7601 npm run dev
```

Open **http://localhost:7602** and sign in with the value of
`RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN` from step 2. Create a bucket and upload a
file to try the service.

| Interface | Default local address | Purpose |
| --- | --- | --- |
| S3 API | `http://localhost:7600` | Object operations and embed URLs |
| Management API | `http://localhost:7601` | Administration and health checks |
| Web console | `http://localhost:7602` | Browser administration and share pages |

To use an S3 client, follow the
[AWS CLI guide](https://openelementslabs.github.io/record-store/guides/aws-cli/) or
an [SDK guide](https://openelementslabs.github.io/record-store/sdk/).

## Install with Docker

Published images are available for `linux/amd64` and `linux/arm64`:

```bash
docker pull ghcr.io/openelementslabs/record-store:latest
docker pull ghcr.io/openelementslabs/record-store-console:latest
```

Pulling the images does not start the service. Follow the
[container deployment guide](https://openelementslabs.github.io/record-store/deployment/container-images/)
to prepare credentials, persistent storage, and the Compose environment file.
For a deployment you intend to keep running, pin a release version or image
digest; `latest` follows the newest stable release.

| Setup | Compose file |
| --- | --- |
| Published server and console images | [`deploy/docker/compose.ghcr.yml`](deploy/docker/compose.ghcr.yml) |
| Build the server from source | [`deploy/docker/compose.yml`](deploy/docker/compose.yml) |
| Build the server and console from source | [`deploy/docker/compose.console.yml`](deploy/docker/compose.console.yml) |

For production, configure TLS and keep the management API private. See the
[production checklist](https://openelementslabs.github.io/record-store/deployment/production-checklist/).
Release checksums, SBOMs, and available provenance attestations are covered in
[Verifying a Release](https://openelementslabs.github.io/record-store/deployment/verifying-releases/).

## S3 compatibility

Record Store implements a subset of the S3 API. Clients need the deployment's S3
endpoint and path-style addressing.

| Area | Supported operations |
| --- | --- |
| Authentication | Signature Version 4 and presigned `GET/PUT` URLs |
| Buckets | Create, list, inspect, and delete empty buckets |
| Objects | Streaming upload and download, metadata inspection, copy, and deletion |
| Listing | Pagination, prefixes, delimiters, and continuation tokens |
| Multipart uploads | Create, upload parts, list, complete, and abort |
| Versioning | Enable or suspend versioning, list versions, read historical versions, and delete markers |
| Retention | Object Lock, legal holds, and bucket retention defaults |
| HTTP behavior | Byte ranges, conditional requests, and per-bucket CORS |

> **Unsupported:** ACLs, `UploadPartCopy`, S3 server-side encryption headers, and
`aws-chunked` trailing-checksum encoding. Unsupported operations or semantic
headers return an S3 XML `NotImplemented` error.

See the [S3 compatibility reference](https://openelementslabs.github.io/record-store/reference/s3-compatibility/)
for exact behavior and client configuration requirements.

Object Lock supports compliance retention, governance retention, and legal holds.
Compliance retention binds every API caller, including root; governance retention
allows an explicitly authorized bypass. These controls do not prevent someone with
access to the data directory from changing or deleting files. See
[Object Lock and Trust](https://openelementslabs.github.io/record-store/security/object-lock/).

## Share and embed links

Both link types grant access to one object and can be revoked. Neither grants
permission to list, upload, or delete objects.

| | Share link | Embed link |
| --- | --- | --- |
| Intended for | A person | A website or application |
| Delivers | A page for viewing or downloading | Read-only object bytes |
| Served by | Web console | S3 endpoint |
| Optional controls | Password, expiry, access count limit | Origin allowlist, expiry |
| Version selection | Current version or a pinned version | Current version or a pinned version |

The console supports previews for selected media and document formats. HTML, SVG,
XML, and scripts are available as downloads only. Browser uploads cannot resume;
an interrupted upload must restart from the beginning.

## Architecture

The S3 and management APIs use a shared service layer. Object payloads live on the
local filesystem under generated identifiers; bucket names and object keys never
become filesystem paths. Metadata lives in embedded databases.

```text
S3 API ───────────┐
                  ├──► Shared service layer ──► Filesystem storage
Management API ───┘             │               (object payloads and
                               │                checksum verification)
                               ▼
                        Metadata catalog
                   (buckets, objects, versions)
```

Writes stream to temporary files, compute checksums, synchronize to disk, and
rename payloads into place before publishing metadata. Recovery journals reconcile
interrupted operations at startup. Crash durability depends on the filesystem
honoring synchronization requests; it does not protect against losing the disk.

The optional web console runs separately and communicates with the management API.
The server remains operable through the CLI and APIs without it.

Read more about [architecture](https://openelementslabs.github.io/record-store/concepts/architecture/)
and [durability](https://openelementslabs.github.io/record-store/concepts/durability/).

## Operations and documentation

| Task | Guide |
| --- | --- |
| Configure the deployment | [Configuration](https://openelementslabs.github.io/record-store/reference/configuration/) and [environment variables](https://openelementslabs.github.io/record-store/reference/environment-variables/) |
| Set up credentials and policies | [Service accounts](https://openelementslabs.github.io/record-store/administration/service-accounts/) and [policies](https://openelementslabs.github.io/record-store/administration/policies/) |
| Retain records | [Object Lock](https://openelementslabs.github.io/record-store/administration/object-lock/) |
| Back up or recover data | [Backup and restore](https://openelementslabs.github.io/record-store/operations/backup-and-restore/) |
| Verify stored data | [Integrity verification](https://openelementslabs.github.io/record-store/operations/integrity-verification/) and [proof bundles](https://openelementslabs.github.io/record-store/reference/proof-bundle/) |
| Share or embed files | [Share links](https://openelementslabs.github.io/record-store/guides/share-links/) and [embed links](https://openelementslabs.github.io/record-store/guides/embed-links/) |
| Diagnose a deployment | [Health and readiness](https://openelementslabs.github.io/record-store/operations/health-and-readiness/) and [troubleshooting](https://openelementslabs.github.io/record-store/troubleshooting/) |

**Backups require the server to be stopped.** They exclude configuration secrets
and the credential master key; preserve those separately.

Full documentation is published at
**https://openelementslabs.github.io/record-store/** and maintained in [`docs/`](docs/).

## Development

Run the Rust checks and release build from the repository root:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features --locked
cargo build --workspace --release --locked
```

After installing the console dependencies, run its checks:

```bash
cd console
npm run lint
npm run typecheck
npm run test
npm run build
```

See [development setup](https://openelementslabs.github.io/record-store/contributing/development-setup/)
and [testing](https://openelementslabs.github.io/record-store/contributing/testing/)
for client compatibility tests, security checks, fuzzing, and end-to-end tests.

### Repository layout

| Directory | Contents |
| --- | --- |
| `apps/` | Server and command-line applications |
| `crates/` | Shared services, protocols, storage, and supporting libraries |
| `console/` | Web console built with Next.js and React |
| `deploy/docker/` | Docker images and Compose configurations |
| `docs/` | MkDocs documentation source |
| `tests/` | Integration, compatibility, and operational checks |
| `.github/workflows/` | CI, documentation, and release pipelines |

### Build the documentation

```bash
pip install --require-hashes -r requirements-docs.txt
mkdocs serve
```

## License

Record Store is maintained by [Open Elements](https://open-elements.com) and
distributed under the [Apache License 2.0](LICENSE).
