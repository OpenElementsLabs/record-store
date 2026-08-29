# First Bucket and Object

This page explains what the [Quick Start](quick-start.md) actually did, and sets up
the credentials an application should use.

## Buckets

A bucket is a named container for objects. Bucket names are validated against
S3-compatible rules: 3–63 bytes, lowercase letters, digits, hyphens and dots,
beginning and ending with a letter or digit, and not written as an IP address.

```bash
record-store bucket create photos
record-store bucket list
```

A few names are reserved: the `xn--` and `sthree-` prefixes, the `-s3alias` and
`--ol-s3` suffixes, and the internal names `record-store-system` and
`record-store-internal`.

A bucket must be empty before it can be deleted.

```bash
record-store bucket delete photos
```

## Objects and keys

An object is a sequence of bytes plus metadata, addressed by a **key** within a bucket.

Keys look like paths but there are no directories. `invoices/2026/03/inv-1.pdf` is a
single key that happens to contain slashes. Listing with a prefix and a delimiter is
what makes it *look* like a folder tree. See
[Buckets and Objects](../concepts/buckets-and-objects.md).

Keys are validated. These are rejected:

| Key | Why |
| --- | --- |
| `/leading` | Leading slash |
| `../secret` | Path traversal |
| `a//b` | Empty segment |
| `a\b` | Backslash |

A bucket name or key never becomes a filesystem path. Payloads are stored under
generated UUIDs.

## Create a service account for your application

Root credentials are bootstrap credentials. Applications should use a **service
account** with only the permissions it needs.

```bash
record-store service-account create my-app
```

```text
Account ID     0195f0c8-....
Access key     SA4D8F0BEE8270423EA5D1
Secret key     <shown once>
```

!!! warning "The secret is shown once"
    Record Store stores signing material encrypted and never returns it again. If you
    lose it, rotate the credential rather than trying to recover it.

A new service account has no policies attached, so it can do nothing until you grant
it something. Create a policy and attach it:

```json title="read-write-photos.json"
{
  "name": "read-write-photos",
  "description": "Full object access to the photos bucket",
  "statements": [
    {
      "effect": "allow",
      "actions": ["s3:ListBucket", "s3:GetObject", "s3:PutObject", "s3:DeleteObject"],
      "resources": ["bucket:photos", "bucket:photos/*"]
    }
  ]
}
```

```bash
record-store policy create ./read-write-photos.json
record-store policy attach <policy-id> <account-id>
```

See [Policies](../administration/policies.md) for the full action and resource model.

## Turn off root S3 access

Once your applications use service accounts, take the root credential off the data
plane:

```bash
RECORD_STORE_ROOT_S3_ENABLED=false
```

Root remains usable for management. See [Authentication](../security/authentication.md).

## Next

- [Application Integration](../guides/application-integration.md) — the architecture to use
- [JavaScript and TypeScript](../sdk/javascript.md) — or [Python](../sdk/python.md),
  [Go](../sdk/go.md), [Rust](../sdk/rust.md)
- [Production Checklist](../deployment/production-checklist.md) — before you go live
