# Buckets and Objects

## Buckets

A bucket is a flat, named namespace for objects. Record Store validates names against
S3-compatible rules so that a name is portable and unambiguous:

- 3 to 63 bytes
- lowercase letters, digits, `-`, and `.` only
- must begin and end with a letter or digit
- must not contain adjacent periods (`..`)
- must not be written as an IPv4 address
- must not use a reserved prefix (`xn--`, `sthree-`), a reserved suffix
  (`-s3alias`, `--ol-s3`), or the internal names `record-store-system` and
  `record-store-internal`

Buckets carry their own settings: [versioning](versioning.md) state,
[quota](../administration/quotas.md), CORS configuration, and
[lifecycle rules](../administration/lifecycle-rules.md).

A bucket must be empty before deletion. Record Store returns `BucketNotEmpty` otherwise.

## Objects

An object is an immutable sequence of bytes plus metadata:

| Field | Meaning |
| --- | --- |
| Key | The object's name within its bucket |
| Size | Length in bytes |
| Checksum | SHA-256, computed while the bytes stream in |
| ETag | S3-compatible entity tag |
| Content type | Declared media type |
| Custom metadata | `x-amz-meta-*` headers, bounded in count and size |
| Version ID | Identifies this particular version |

Objects are immutable. Writing to the same key does not modify bytes in place: it
creates a new version, and — depending on the bucket's versioning state — either
replaces the current one or adds to its history.

## Keys are not paths

A key is one opaque string. `reports/2026/q1.pdf` contains slashes, but there is no
directory called `reports`, nothing to create, and nothing to remove when the last
object under it is deleted.

Prefixes and delimiters are what produce a folder-like view:

```bash
aws --endpoint-url http://127.0.0.1:7600 s3api list-objects-v2 \
  --bucket demo --prefix reports/ --delimiter /
```

That returns objects directly under `reports/` plus a list of *common prefixes* —
the pseudo-folders. The console's file browser is built on exactly this.

### Key validation

Record Store rejects keys that would be ambiguous or unsafe:

| Rejected | Reason |
| --- | --- |
| Empty key | No name |
| `/report.pdf` | Leading slash |
| `a/../b` | Path traversal |
| `a//b` | Empty path segment |
| `a\b` | Backslash |
| Keys containing control characters | Not representable safely |

This is enforced by the domain type, before a key reaches storage or metadata.

## Nothing becomes a filesystem path

Bucket names and object keys are never used to build file paths. Payloads are written
under generated UUIDs:

```text
<data-directory>/objects/<2 hex>/<2 hex>/<object UUID>
```

The mapping from a logical key to a payload lives in the metadata catalog. A key
containing `../` could not escape anything even if validation were bypassed, because
the key is never part of a path in the first place.

## How a write becomes durable

An upload streams through bounded chunks into a create-only temporary file while
SHA-256 and MD5 are computed. The file is `fsync`ed and atomically renamed into place,
and only then is metadata published.

A durable publication journal records the window between writing bytes and publishing
metadata, so a crash in between is resolved on the next startup rather than leaving an
orphaned payload or a metadata entry pointing at nothing.

!!! note "Keep the temporary directory on the same filesystem"
    Publication relies on an atomic rename. If `storage.temporary_directory` is on a
    different filesystem from `storage.data_directory`, the rename becomes a copy and
    the guarantee is lost.
