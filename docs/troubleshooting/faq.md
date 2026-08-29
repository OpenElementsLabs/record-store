# FAQ

## General

**Is Record Store a drop-in replacement for S3?**

For the [supported operations](../reference/s3-compatibility.md), yes — point your SDK
at the endpoint and use path-style addressing. Access control lists, Object Lock,
`UploadPartCopy`, and server-side-encryption request headers are not supported.

**Do I need a cluster?**

No. Standalone is a first-class deployment. Use a cluster when you need to survive
losing a machine, and accept the operational weight that comes with it.

**Which port do applications use?**

`7600`, the S3 API. `7601` is administration and must not be public.

**What is the difference between the S3 API and the management API?**

The S3 API moves data and is authenticated with SigV4 credentials. The management API
administers the deployment and is authenticated with bearer tokens. A management token
cannot read an object; an S3 credential cannot change configuration.

## Credentials

**I lost a secret key. Can I recover it?**

No. Record Store stores a sealed form it cannot reverse. Rotate:

```bash
record-store credential rotate <account-id> --endpoint <endpoint>
```

**Can I rotate the credential master key?**

No. It seals credentials, webhook secrets, capability secrets, and — with encryption on
— per-object data keys. Replacing it makes all of that permanently unreadable. Generate
it once and back it up separately from the data directory.

**Why does creating a service account fail?**

`auth.credential_master_key` is not set. Credentials are stored sealed under it, and
Record Store will not fall back to storing them any other way.

**Does rotating disable the old credential?**

No. Rotation issues a **new** credential alongside the old one, which is what makes a
zero-downtime rotation possible. Disable the old one explicitly once traffic has moved.

## Access control

**Why does my policy not work?**

Almost always the resource pattern. `bucket:uploads` covers the bucket;
`bucket:uploads/*` covers its objects. Listing a bucket *and* reading its objects needs
both.

**Can I use IAM-style wildcards?**

Only a single trailing `*`. `bucket:uploads/*` is fine; `bucket:*/logs/*` is rejected.

**Can I grant an action on all buckets?**

`bucket:*` matches everything. Use it sparingly.

**Are there session tokens?**

No. Temporary credentials are plain access-key/secret-key pairs with an expiry. Do not
set `AWS_SESSION_TOKEN`.

## Storage

**Is encryption at rest on by default?**

No. Set `RECORD_STORE_STORAGE_ENCRYPTION_ENABLED=true`, which also requires the master
key. Service-account credentials, webhook secrets, and capability secrets are **always**
sealed regardless of that setting.

**Are object keys encrypted?**

No. If your key names are themselves sensitive, encode them rather than putting the
sensitive value in the key.

**Enabling encryption did nothing to existing objects — why?**

It applies to newly committed payloads. Existing objects stay in plaintext and both are
readable side by side. Rewrite objects to encrypt them.

**Why is disk usage higher than the sum of my objects?**

Version history and in-progress multipart uploads. Compare logical and physical bytes
with `record-store storage inspect`.

**Can I turn versioning off once it is on?**

You can `suspend` it, which stops new versions while keeping existing history.
`enabled` cannot return to `disabled` — that would silently discard history.

## Sharing

**What is the difference between share links, embed links, and presigned URLs?**

| | For | Served on | Revocable |
| --- | --- | --- | --- |
| Share link | A person | Console `/s/<token>` | Yes |
| Embed link | A website | Storage `/e/<token>` | Yes |
| Presigned URL | An automated client | Storage | **No** — only expiry |

A presigned URL cannot be revoked once issued. Share and embed links can, and the
revocation takes effect on the next request.

**Why do my links point at `127.0.0.1`?**

Set `RECORD_STORE_SHARING_SHARE_BASE_URL` and `RECORD_STORE_SHARING_EMBED_BASE_URL`.
Record Store cannot infer its public hostname from behind a proxy.

**Can I turn sharing off?**

```bash
RECORD_STORE_SHARING_SHARES_ENABLED=false
RECORD_STORE_SHARING_EMBEDS_ENABLED=false
```

## Operations

**How do I back up?**

`record-store server backup-metadata` for metadata, your usual file backup for
`objects/`. Both from the same point in time, plus the master key kept separately. See
[Backup and Restore](../operations/backup-and-restore.md).

**Can I back up while the server runs?**

Not with `backup-metadata` — it takes the exclusive data lock. Either stop the server
briefly, or take a filesystem snapshot and run the backup against that.

**Is the audit log pruned?**

No. It grows with request volume and has no automatic retention. Budget for it.

**How do I see who deleted something?**

```bash
record-store audit --operation "s3:DELETE" --limit 100 --endpoint <endpoint>
```

**Can I change configuration without a restart?**

No. Configuration is read at startup. Validate first with
`record-store server check-config`.

## Cluster

**How many nodes do I need?**

Three storage nodes minimum. Consensus needs a majority, and two voters tolerate no
failures at all — worse than one.

**Can I change the replication factor later?**

It is fixed when the cluster is initialized. Setting it on a joining node has no effect.

**Why did nothing move to my new node?**

Automatic rebalancing is off by default. New writes use the node immediately; run
`record-store rebalance start` to move existing data.

**A node died. What do I do?**

Wait for it to reach `offline` — repair is already restoring redundancy. Then
decommission it with `--force` and join a replacement with an empty data directory.

**Does erasure coding exist?**

No. Replication is the durability model. `GET /api/v1/system/info` reports
`capabilities.erasure_coding` as `false`, and multi-region conflict resolution is
likewise not implemented.

## Development

**Where do I start?**

[Development Setup](../contributing/development-setup.md).

**How do I run the compatibility tests?**

```bash
bash tests/compatibility/run.sh
```

They exercise real AWS SDKs against a running server. See
[Testing](../contributing/testing.md).
