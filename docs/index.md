# Record Store

Record Store is a self-hosted, S3-compatible object storage service written in Rust.
It runs as a single process on one server.

Applications talk to it with the AWS SDKs they already use. Administrators manage it
through a web console, a command-line tool, or a native HTTP API.

<div class="grid cards" markdown>

-   :material-rocket-launch: **Try it**

    ---

    Run Record Store and store your first object in about five minutes.

    [:octicons-arrow-right-24: Quick Start](getting-started/quick-start.md)

-   :material-application-brackets: **Use it in an application**

    ---

    Connect an application with the AWS SDK, and upload from a browser safely.

    [:octicons-arrow-right-24: Application Integration](guides/application-integration.md)

-   :material-server: **Deploy it**

    ---

    Docker, Docker Compose, or Coolify, with TLS and persistent storage.

    [:octicons-arrow-right-24: Deployment](deployment/index.md)

-   :material-lifebuoy: **Something is broken**

    ---

    Signature errors, upload failures, and proxy problems.

    [:octicons-arrow-right-24: Troubleshooting](troubleshooting/index.md)

</div>

## What Record Store does

**S3-compatible API.** AWS Signature Version 4, presigned URLs, multipart uploads,
versioning, range and conditional reads, and per-bucket CORS. See
[S3 Compatibility](reference/s3-compatibility.md) for the exact surface.

**One process to run.** A single binary with a single data directory. No external
database, no message broker, and no coordination service to operate.

**Encryption at rest.** Optional chunked AES-256-GCM with a per-object data key
wrapped by a master key you supply and control.

**Credentials and policies.** Service accounts with rotatable credentials, allow/deny
policies, temporary credentials, and management roles separate from S3 access.

**Sharing.** Share links give a person read access to one object through a Record Store
page. Embed links give a website a read-only URL for the bytes. Both are revocable
capabilities, never credentials.

**Operations.** Durable audit trail, storage events with signed webhooks, Prometheus
metrics, lifecycle expiration, integrity verification, and offline metadata backup.

## What Record Store does not do

Being precise about this is more useful than a longer feature list.

- **A deployment is one process on one machine.** Durability is whatever the storage
  underneath it gives you, so use redundant disks and take
  [backups](operations/backup-and-restore.md). If the machine is gone, the service is
  down until you restore it.
- **ACLs, Object Lock, `UploadPartCopy`, server-side encryption headers, and AWS
  `aws-chunked` trailing checksums are not implemented.** Unsupported operations
  return an S3 `NotImplemented` error rather than being silently accepted.
- **Browser uploads through the console are not resumable.** An interrupted upload
  must be sent again from the first byte.

## Where to go next

| You are | Start here |
| --- | --- |
| Evaluating Record Store | [Introduction](getting-started/index.md) |
| Writing an application against it | [Application Integration](guides/application-integration.md) |
| Deploying it | [Deployment Overview](deployment/index.md) |
| Running it already | [Administration](administration/index.md) and [Operations](operations/index.md) |
| Contributing to it | [Development Setup](contributing/development-setup.md) |

## License

Record Store is licensed under the Apache License 2.0. The full text ships with
the source and inside the published container images at
`/usr/share/licenses/record-store/LICENSE`.
