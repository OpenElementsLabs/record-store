# Durability

Durability is the question "if this write returned success, what have I actually been
promised?" This page answers it precisely, including where the promise stops.

## What a successful write means

The payload was streamed to a temporary file, checksummed, `fsync`ed, and atomically
renamed into place. Metadata was published afterwards, so a partially written payload
is never visible as an object.

That gives you two guarantees and one clear limit:

| | |
| --- | --- |
| Survives a process crash | Yes — the rename is atomic, and a partial payload is never published |
| Survives power loss | Yes, to the extent your filesystem and disk honour `fsync` |
| Survives losing the disk | **No** |

There is one copy of your data, on one machine. Everything below follows from that.

## Redundancy is the storage layer's job

Record Store does not make a second copy of a payload for you, so the redundancy under
the data directory is the redundancy you have:

- Put the data directory on redundant storage — RAID, a mirrored ZFS pool, or a
  replicated volume from your hypervisor or cloud provider.
- Expect a lost disk on non-redundant storage to mean a restore from backup.
- Expect a lost machine to mean downtime until you restore it somewhere else.

Sizing that storage is covered in
[Capacity Planning](../operations/capacity-planning.md).

## Integrity

Every payload carries a SHA-256 checksum computed while the bytes streamed in, and
ordinary reads check the stored bytes against it rather than trusting them. What that
means exactly depends on the read, and the difference is worth stating:

| Read | What is checked, and when |
| --- | --- |
| Whole object or version, unencrypted | **Length before any byte is sent**, then the SHA-256 recomputed while streaming. A mismatch fails the read before its last chunk. |
| Whole object or version, encrypted | Length, and then each chunk's authentication tag as it is decrypted. A damaged chunk fails before it is sent. |
| Ranged read, unencrypted | **Length before any byte is sent.** A digest over part of a payload cannot be compared with a digest over all of it. |
| Ranged read, encrypted | Length, and then each chunk's authentication tag as it is decrypted — so a ranged read is covered too. |
| Multipart assembly | Each part against the checksum recorded when that part was uploaded. |
| Server-side copy | The source is read as a whole object, and the destination refuses a write whose bytes do not match the source's checksum. |

Two limits stated plainly:

- For an unencrypted whole-object read this is **detection, not prevention**. Bytes have already
  left by the time the digest disagrees; the guarantee is that the read *fails*
  rather than completing silently. Verifying before releasing anything would mean
  reading every payload twice.
- For an unencrypted ranged read, a same-length edit inside the requested range is
  not detected by the read itself. It is detected by
  [integrity verification](../operations/integrity-verification.md), which reads the
  whole object. Enabling
  [encryption at rest](../security/encryption.md) closes the gap, because each chunk
  is authenticated independently.

Verification detects corruption; it does not fix it. A payload that fails verification
is restored from a [backup](../operations/backup-and-restore.md).

## Metadata

Object and bucket metadata lives in a durable local catalog with ordered migrations.
It is small relative to the payloads and is the part you cannot reconstruct from the
objects alone, which is why
[backup and restore](../operations/backup-and-restore.md) treats it as its own
concern.

## What durability does not cover

!!! danger "Redundant storage is not backup"
    RAID and replicated volumes faithfully reproduce whatever you asked for, including
    a deletion or a mistaken overwrite. They protect against hardware failure, not
    against you.

    Take [backups](../operations/backup-and-restore.md), and consider
    [versioning](versioning.md) so an overwrite is recoverable.

!!! info "Erasure coding is not implemented"
    The domain model reserves an erasure-coded profile and the repository contains an
    unused `record-store-erasure` crate, but no code path produces or reads erasure
    stripes. Do not plan capacity around erasure coding.
