# Migrating from MinIO

This page is for moving an existing MinIO deployment's data and clients onto
Record Store. It covers the ports, the credentials, the client settings that have
to change, what Record Store supports, and — at the end, in full — what it does
not do.

Read the last section first if you are evaluating rather than committed. Record
Store is a records store built around one authoritative copy on one node. If your
deployment depends on multi-node replication or erasure coding, Record Store will
not replace it, and finding that out on this page is cheaper than finding it out
halfway through a copy.

## Ports

| | MinIO (default) | Record Store (default) |
| --- | --- | --- |
| S3 API | 9000 | **7600** |
| Web console | 9001 | **7602** |
| Management API | — served on the S3 port | **7601**, a separate listener |
| Internal cluster RPC | — | 7603 |

The management plane being its own listener is the difference that matters when you
write firewall rules. Administration does not travel over the S3 port, so port 7600
can be published while 7601 stays on a private network. Only `GET /health` and
`GET /ready` are public; `GET /metrics` takes its own dedicated token.

Every listener is configurable:

```bash
export RECORD_STORE_S3_BIND="0.0.0.0:9000"    # if you must keep the old port
export RECORD_STORE_API_BIND="127.0.0.1:7601"
```

See [Ports](../reference/ports.md).

## Credentials

| MinIO | Record Store |
| --- | --- |
| `MINIO_ROOT_USER` | `RECORD_STORE_ROOT_ACCESS_KEY` |
| `MINIO_ROOT_PASSWORD` | `RECORD_STORE_ROOT_SECRET_KEY` |
| — | `RECORD_STORE_CREDENTIAL_MASTER_KEY` **(required)** |
| — | `RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN` |

Two of these have no MinIO equivalent and neither is optional in practice.

**`RECORD_STORE_CREDENTIAL_MASTER_KEY`** is injected, never stored by Record Store,
and is what encrypts service-account secrets, webhook signing secrets, capability
tokens, and — when enabled — object payloads. It also derives the key that signs
proof bundles. Keep it stable and back it up somewhere other than the data
directory: lose it and the encrypted material is unreadable. Startup refuses a
missing or mismatched key rather than quietly degrading.

**`RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN`** authenticates the management plane. The
management roles are separate from S3 policies: system administrator, storage
administrator, and a read-only auditor.

```bash
export RECORD_STORE_ROOT_ACCESS_KEY='local-admin'
export RECORD_STORE_ROOT_SECRET_KEY='replace-with-a-long-random-secret'
export RECORD_STORE_CREDENTIAL_MASTER_KEY='replace-with-a-stable-32-byte-or-longer-key'
export RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN='replace-with-a-distinct-32-byte-or-longer-token'
export RECORD_STORE_STORAGE_ENCRYPTION_ENABLED=true
```

### After the move, stop using root on the data plane

Root exists to bootstrap. Create a service account, attach a policy, and turn root
off for S3:

```bash
record-store service-account create my-app
record-store policy create ./policy.json
record-store policy attach <policy-id> <account-id>
export RECORD_STORE_ROOT_S3_ENABLED=false
```

Policies are Record Store's own allow/deny model, not IAM JSON — explicit deny wins,
no matching allow is an implicit deny, and resources use canonical decoded keys with
only a trailing wildcard. See [Policies](../administration/policies.md).

## Client configuration

Three settings usually need changing. All three are the same ones the
[AWS CLI guide](../guides/aws-cli.md) describes.

**Path-style addressing is required.** Virtual-hosted style
(`https://bucket.storage.example.com/key`) is not supported.

**Disable `aws-chunked` trailing checksums.** Record Store reports that encoding as
unsupported rather than silently accepting it, so newer AWS SDKs need telling not to
send it.

**Point at the endpoint explicitly**, and set a region — any region string works;
SigV4 just needs one to sign with.

```bash
export AWS_ACCESS_KEY_ID="$RECORD_STORE_ROOT_ACCESS_KEY"
export AWS_SECRET_ACCESS_KEY="$RECORD_STORE_ROOT_SECRET_KEY"
export AWS_DEFAULT_REGION=us-east-1
export AWS_EC2_METADATA_DISABLED=true
export AWS_REQUEST_CHECKSUM_CALCULATION=WHEN_REQUIRED
export AWS_RESPONSE_CHECKSUM_VALIDATION=WHEN_REQUIRED
aws configure set s3.addressing_style path

aws --endpoint-url http://localhost:7600 s3api list-buckets
```

With a named profile, apply the addressing style to that profile too:
`aws configure set s3.addressing_style path --profile PROFILE`.

### `rclone`

Set the listing version explicitly. Record Store implements `ListObjectsV2` only,
and `rclone` will otherwise choose for itself based on provider detection:

```ini
[recordstore]
type = s3
provider = Other
endpoint = http://localhost:7600
access_key_id = ...
secret_access_key = ...
region = us-east-1
force_path_style = true
list_version = 2
```

### A note on `mc`

MinIO's `mc` is not tested against Record Store. Its administrative commands target
MinIO's own admin API, which Record Store does not implement, and its listing
behaviour is not something this project verifies. Use it to read *from* MinIO if you
like; use `aws-cli` or `rclone` for the side that talks to Record Store.

The clients this project does test on every change are boto3, the AWS SDK for
JavaScript v3, the AWS SDK for Go, and the AWS SDK for Java v2. See
[Testing](../contributing/testing.md).

## Moving the data

Create the buckets first — Record Store does not create one implicitly — then copy.

```bash
# Buckets
aws --endpoint-url http://localhost:7600 s3api create-bucket --bucket records

# Enable versioning before copying if you want history from here on.
aws --endpoint-url http://localhost:7600 s3api put-bucket-versioning \
  --bucket records --versioning-configuration Status=Enabled

# Copy, reading from MinIO and writing to Record Store.
rclone sync minio:records recordstore:records --progress
```

Two things to decide before you start:

- **Versioning is per bucket and off by default.** Turn it on before the copy if you
  want every later write to keep history. A copy into a versioned bucket does not
  reconstruct the source's version history — it writes current objects as new
  versions here.
- **Object Lock can only be enabled when a bucket is created.** If the data needs
  retention, create the bucket with lock enabled up front:

  ```bash
  aws --endpoint-url http://localhost:7600 s3api create-bucket \
    --bucket records --object-lock-enabled-for-bucket
  ```

  Enabling it later is refused, deliberately: it would claim protection over versions
  written without it. See [Object Lock](../administration/object-lock.md).

### Checking the copy

Every payload is checksummed on write and verified on read, so a corrupted transfer
fails rather than being stored. To check explicitly:

```bash
record-store verify bucket records
record-store verify object records reports/2026-q1.pdf
```

## What is supported

| Capability | Notes |
| --- | --- |
| SigV4 header authentication | Header and presigned GET/PUT |
| Buckets | Create, head, list, delete when empty |
| Objects | Streaming put, get, head, idempotent delete |
| `ListObjectsV2` | Prefix, delimiter, bounded pagination, continuation tokens |
| Multipart upload | Create, upload part, list parts, complete, abort, list uploads |
| Versioning | Enabled, suspended, delete markers, `ListObjectVersions` |
| Object Lock | `GOVERNANCE`/`COMPLIANCE` retention, legal holds, bucket defaults |
| `CopyObject` | Same-bucket and cross-bucket, `COPY` and `REPLACE` directives |
| Ranges and conditionals | `Range`, `If-Match`, `If-None-Match`, `If-Modified-Since`, `If-Unmodified-Since` |
| CORS | Per bucket, with unsigned browser preflights |
| Checksums | `x-amz-content-sha256`, single-part and multipart ETags |
| Metadata | Content type and `x-amz-meta-*` |
| Encryption at rest | Deployment-wide, AES-256-GCM, off by default |
| Quotas and lifecycle expiration | Per bucket, with a supervised worker |
| Share and embed links | Capability URLs for one object, revocable |

The machine-checked list lives in
[S3 Compatibility](../reference/s3-compatibility.md) and is kept in step with the
routing and the protocol tests rather than maintained separately.

## What Record Store does not do

Unsupported operations return S3 XML `NotImplemented`. They are never silently
accepted, so a client learns immediately rather than discovering later that a header
it sent was ignored.

**Storage architecture**

- **No clustering, replication, or erasure coding.** One process, one machine, one
  copy of your data. Redundancy under the data directory is the redundancy you have:
  use RAID, a mirrored pool, or a replicated volume, and take backups. If this is the
  blocker, it is the blocker — replication is substantial work this project intends
  to fund with adoption rather than ship ahead of it.
- **No bucket-to-bucket or site-to-site replication.**
- **No storage tiering or lifecycle transitions.** Lifecycle rules expire objects and
  non-current versions; they do not move data between classes.

**S3 API surface**

- **No `ListObjects` V1.** Only `ListObjectsV2` — a request without `list-type=2`
  returns `NotImplemented`. This is the most common thing to trip over with an older
  tool.
- **No `DeleteObjects` batch delete.** Delete objects one request at a time.
- **No `UploadPartCopy`.** Download and re-upload the part.
- **No ACLs.** Use [policies](../administration/policies.md).
- **No S3 bucket policies or IAM policy documents.** Record Store has its own policy
  model; an IAM JSON document will not be accepted.
- **No server-side encryption request headers.** Encryption at rest is a deployment
  setting, not a per-request one.
- **No `aws-chunked` transfer encoding or trailing checksums.**
- **No object tagging**, and no `x-amz-tagging` on write.
- **No S3 bucket notifications.** Storage events are delivered by
  [signed webhooks](../administration/events-and-webhooks.md) instead.
- **No S3 Select, no object Lambda, no static website hosting.**
- **No virtual-hosted-style addressing.** Path-style only.
- **No STS or session tokens.** [Temporary credentials](../administration/temporary-credentials.md)
  are plain expiring key pairs, not session tokens.

**Operational**

- **No MinIO admin API**, and therefore no `mc admin`. Administration is the
  management API on 7601, the CLI, or the console.
- **No browser-resumable uploads through the console.** An interrupted upload is sent
  again from the first byte.
- **The tamper-evident audit chain is not shipped yet.** The audit trail is durable
  and queryable, but a hash chain, checkpoints, and external anchoring are still in
  progress. Proof bundles carry the section and report it as unavailable rather than
  implying it is covered. Until then, what the audit trail reports is what the server
  says, not evidence about the past.

## After the move

- Work through the [Security Checklist](../security/checklist.md).
- Set `RECORD_STORE_ROOT_S3_ENABLED=false` once service accounts are in place.
- Decide backups. Object Lock refuses deletions; it does not survive losing the
  disk. See [Backup and Restore](../operations/backup-and-restore.md).
- Configure CORS on any bucket a browser must reach. Browser access is denied by
  default and there is no deployment-wide wildcard.
