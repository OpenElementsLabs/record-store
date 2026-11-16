# Migrating from MinIO

This page shows you how to move data and clients from an existing [MinIO](https://www.min.io/)
deployment to Record Store. It covers ports, credentials, client settings, and
supported features. The last section lists what Record Store doesn't support.

!!! note "Evaluating Record Store? Start with the limitations"
  Record Store keeps one authoritative copy of your data on one node. If your deployment depends on multi-node replication or erasure coding, Record Store can't replace it. Before you start a migration, read [Unsupported features](#unsupported-features).

## Ports

The following table compares the default ports:

| Listener | MinIO | Record Store |
| --- | --- | --- |
| S3 API | 9000 | **7600** |
| Web console | 9001 | **7602** |
| Management API | Shares the S3 port | **7601**, a separate listener |
| Internal cluster RPC | — | 7603 |

The separate management listener affects your firewall rules. Administration
traffic doesn't use the S3 port, so you can publish port 7600 and keep port 7601
on a private network. On port 7601, only `GET /health` and `GET /ready` are
public. `GET /metrics` requires its own token.

You can change the address of every listener. For example, to keep the MinIO S3
port, set the following variables:

```bash
export RECORD_STORE_S3_BIND="0.0.0.0:9000"
export RECORD_STORE_API_BIND="127.0.0.1:7601"
```

For more information, see [Ports](../reference/ports.md).

## Credentials

The following table maps MinIO credential variables to Record Store variables:

| MinIO | Record Store |
| --- | --- |
| `MINIO_ROOT_USER` | `RECORD_STORE_ROOT_ACCESS_KEY` |
| `MINIO_ROOT_PASSWORD` | `RECORD_STORE_ROOT_SECRET_KEY` |
| — | `RECORD_STORE_CREDENTIAL_MASTER_KEY` **(required)** |
| — | `RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN` |

Two variables have no MinIO equivalent. In practice, you need both.

- **`RECORD_STORE_CREDENTIAL_MASTER_KEY`** encrypts service-account secrets,
  webhook signing secrets, capability tokens, and, if you turn on encryption,
  object payloads. Record Store also derives the key that signs proof bundles
  from it. You provide this key at startup, and Record Store never stores it.
  Keep the key stable, and back it up outside the data directory. If you lose
  the key, Record Store can't read the encrypted data. If the key is missing or
  doesn't match, Record Store doesn't start.
- **`RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN`** authenticates requests to the
  management API. Management roles are separate from S3 policies. The roles are
  system administrator, storage administrator, and read-only auditor.

To set the credentials and turn on encryption at rest, export the following
variables:

```bash
export RECORD_STORE_ROOT_ACCESS_KEY='ROOT_ACCESS_KEY'
export RECORD_STORE_ROOT_SECRET_KEY='ROOT_SECRET_KEY'
export RECORD_STORE_CREDENTIAL_MASTER_KEY='CREDENTIAL_MASTER_KEY'
export RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN='MANAGEMENT_SYSTEM_TOKEN'
export RECORD_STORE_STORAGE_ENCRYPTION_ENABLED=true
```

Replace the following:

- `ROOT_ACCESS_KEY`: the access key ID for the root S3 credentials.
- `ROOT_SECRET_KEY`: a long, random secret key.
- `CREDENTIAL_MASTER_KEY`: a stable key of 32 or more bytes.
- `MANAGEMENT_SYSTEM_TOKEN`: a separate token of 32 or more bytes.

### Stop using root credentials for S3 after the migration

The root credentials exist to set up Record Store. After the migration, create a
service account, attach a policy to it, and turn off root access to the S3 API:

```bash
record-store service-account create my-app
record-store policy create ./policy.json
record-store policy attach POLICY_ID ACCOUNT_ID
export RECORD_STORE_ROOT_S3_ENABLED=false
```

Replace `POLICY_ID` and `ACCOUNT_ID` with the IDs that the first two commands
return.

Record Store policies use their own allow-and-deny model, not IAM JSON:

- An explicit deny overrides any allow.
- If no statement allows a request, Record Store denies it.
- Resources use canonical decoded keys, and a wildcard is allowed only at the
  end.

For more information, see [Policies](../administration/policies.md).

## Configure your clients

You usually need to change three client settings. The
[AWS CLI guide](../guides/aws-cli.md) describes the same settings.

- **Use path-style addressing.** Record Store doesn't support virtual-hosted
  style addressing, such as `https://bucket.storage.example.com/key`.
- **Turn off `aws-chunked` trailing checksums.** Record Store rejects this
  encoding as unsupported. Recent AWS SDKs use it by default, so you must turn
  it off.
- **Set the endpoint and a region explicitly.** Any region string works,
  because SigV4 needs a region only to sign requests.

The following example configures the AWS CLI and lists your buckets:

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

If you use a named profile, set the addressing style for that profile:

```bash
aws configure set s3.addressing_style path --profile PROFILE
```

Replace `PROFILE` with the name of your profile.

### Configure rclone

Record Store supports only `ListObjectsV2`. If you don't set the listing version,
`rclone` chooses one based on the provider it detects. Set `list_version = 2` in
your remote configuration:

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

### Don't use mc with Record Store

This project doesn't test MinIO's `mc` client against Record Store. The `mc`
administration commands use the MinIO admin API, which Record Store doesn't
implement, and this project doesn't verify the `mc` listing behavior. You can
use `mc` to read from MinIO, but use `aws-cli` or `rclone` to write to Record
Store.

This project tests the following clients on every change:

- boto3
- AWS SDK for JavaScript v3
- AWS SDK for Go
- AWS SDK for Java v2

For more information, see [Testing](../contributing/testing.md).

## Copy the data

Record Store doesn't create buckets automatically. Before you copy any data,
decide how to configure each bucket:

- **Versioning.** Versioning is set for each bucket and is off by default. If you
  want to keep the history of later writes, turn on versioning before you copy.
  A copy doesn't reproduce the version history from MinIO. Record Store writes
  each current object as a new version.
- **Object Lock.** You can turn on Object Lock only when you create a bucket.
  Record Store refuses to turn it on later, because the lock would claim to
  protect versions that were written without it. For more information, see
  [Object Lock](../administration/object-lock.md).

To create a bucket, turn on versioning, and copy the data from MinIO, run the
following commands:

```bash
aws --endpoint-url http://localhost:7600 s3api create-bucket --bucket records

aws --endpoint-url http://localhost:7600 s3api put-bucket-versioning \
  --bucket records --versioning-configuration Status=Enabled

rclone sync minio:records recordstore:records --progress
```

If the data needs a retention period, create the bucket with Object Lock
instead:

```bash
aws --endpoint-url http://localhost:7600 s3api create-bucket \
  --bucket records --object-lock-enabled-for-bucket
```

### Verify the copy

Record Store checksums every payload when it writes the payload and verifies the
checksum when it reads the payload. If a transfer corrupts a payload, the write
fails and Record Store doesn't store the payload.

To verify a bucket or a single object, run the following commands:

```bash
record-store verify bucket records
record-store verify object records reports/2026-q1.pdf
```

## Supported features

The following table lists the S3 features that Record Store supports:

| Feature | Details |
| --- | --- |
| SigV4 authentication | Header authentication, and presigned GET and PUT requests. |
| Buckets | Create, head, list, and delete when empty. |
| Objects | Streaming put, get, head, and idempotent delete. |
| `ListObjectsV2` | Prefix, delimiter, bounded pagination, and continuation tokens. |
| Multipart upload | Create, upload part, list parts, complete, abort, and list uploads. |
| Versioning | Enabled and suspended states, delete markers, and `ListObjectVersions`. |
| Object Lock | `GOVERNANCE` and `COMPLIANCE` retention, legal holds, and bucket defaults. |
| `CopyObject` | Within a bucket and across buckets, with `COPY` and `REPLACE` directives. |
| Ranges and conditional requests | `Range`, `If-Match`, `If-None-Match`, `If-Modified-Since`, and `If-Unmodified-Since`. |
| CORS | Configured for each bucket, with unsigned browser preflight requests. |
| Checksums | `x-amz-content-sha256`, and single-part and multipart ETags. |
| Metadata | Content type and `x-amz-meta-*` headers. |
| Encryption at rest | AES-256-GCM for the whole deployment. Off by default. |
| Quotas and lifecycle expiration | Configured for each bucket, with a supervised worker. |
| Share links and embed links | Revocable capability URLs for one object. |

For the complete list, see [S3 compatibility](../reference/s3-compatibility.md).
That list is machine-checked against the request routing and the protocol
tests, so it stays current.

## Unsupported features

Record Store returns an S3 XML `NotImplemented` error for every unsupported
operation. It never accepts an unsupported request silently, so a client knows
immediately when Record Store doesn't support a header or operation.

### Storage architecture

- **No clustering, replication, or erasure coding.** Record Store runs one
  process on one machine and keeps one copy of your data. Your only redundancy
  is the storage under the data directory. Use RAID, a mirrored pool, or a
  replicated volume, and take backups. The project plans to fund replication
  through adoption, not to ship it beforehand.
- **No bucket-to-bucket or site-to-site replication.**
- **No storage tiering or lifecycle transitions.** Lifecycle rules expire objects
  and noncurrent versions. They don't move data between storage classes.

### S3 API

- **No `ListObjects` V1.** Record Store supports only `ListObjectsV2`. A request
  without `list-type=2` returns `NotImplemented`. Older tools often fail
  because of this limitation.
- **No `DeleteObjects` batch delete.** Delete objects one request at a time.
- **No `UploadPartCopy`.** Download the part and upload it again.
- **No ACLs.** Use [policies](../administration/policies.md) instead.
- **No S3 bucket policies or IAM policy documents.** Record Store uses its own
  policy model and rejects IAM JSON documents.
- **No server-side encryption request headers.** Encryption at rest is a
  deployment setting, not a per-request setting.
- **No `aws-chunked` transfer encoding or trailing checksums.**
- **No object tagging**, including `x-amz-tagging` on writes.
- **No S3 bucket notifications.** Record Store delivers storage events as
  [signed webhooks](../administration/events-and-webhooks.md) instead.
- **No S3 Select, S3 Object Lambda, or static website hosting.**
- **No virtual-hosted-style addressing.** Use path-style addressing.
- **No STS or session tokens.**
  [Temporary credentials](../administration/temporary-credentials.md) are
  key pairs that expire, not session tokens.

### Operations

- **No MinIO admin API**, so `mc admin` doesn't work. To administer Record
  Store, use the management API on port 7601, the CLI, or the console.
- **No resumable browser uploads in the console.** If an upload is interrupted,
  the console sends the file again from the first byte.
- **No tamper-evident audit chain yet.** The audit trail is durable and you can
  query it, but the hash chain, checkpoints, and external anchoring are still in
  development. Proof bundles include the audit-chain section and report it as
  unavailable. Until the audit chain ships, the audit trail records what the
  server reports. It isn't evidence about past events.

## What's next

After the migration, complete the following tasks:

- Work through the [Security checklist](../security/checklist.md).
- After your applications use service accounts, set
  `RECORD_STORE_ROOT_S3_ENABLED=false`.
- Plan backups. Object Lock prevents deletion, but it doesn't protect your data
  if you lose the disk. For more information, see
  [Backup and restore](../operations/backup-and-restore.md).
- Configure CORS for every bucket that a browser must access. Record Store
  denies browser access by default and has no deployment-wide wildcard.
