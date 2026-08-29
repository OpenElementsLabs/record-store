# Persistent Storage

Everything durable lives under one directory, `storage.data_directory`. In the
container image that is `/var/lib/record-store`.

## Layout

```text
<data_directory>/
├── .record-store.lock       exclusive lock — one process per directory
├── metadata/
│   ├── catalog.redb         buckets, objects, versions, multipart state
│   ├── credentials.redb     service accounts, credentials, policies
│   ├── audit.redb           the audit trail
│   ├── events.redb          storage events and webhook state
│   ├── lifecycle.redb       lifecycle scan cursors
│   ├── sharing.redb         share and embed capabilities
│   └── consensus/           Raft log and snapshots — cluster mode
├── objects/                 object payloads
├── system/                  internal storage bookkeeping
├── tmp/                     incomplete payloads
└── node-identity.json       this node's identity — cluster mode
```

Two things follow from this layout:

- **`metadata/` and `objects/` are one unit.** Object payloads are meaningless without
  the catalog that names them. Never back up or restore one without the other.
- **`tmp/` is disposable but must be on the same filesystem as `objects/`.** Committing
  an upload is a rename, and a rename across filesystems is a copy. Splitting them
  costs a full extra write per upload.

## Requirements

| | |
| --- | --- |
| Filesystem | POSIX with working `fsync` and atomic rename — ext4, XFS, ZFS, APFS |
| Mode | Read-write, owned by the running user (uid `10001` in the image) |
| Exclusivity | One process per directory, enforced by `.record-store.lock` |

!!! danger "Not on NFS, SMB, or a network filesystem"
    Durability rests on `fsync` and atomic rename behaving as POSIX specifies. Network
    filesystems commonly do not, which turns a reported-durable write into a lost one
    after a crash. Use block storage.

    The lock file is also unreliable there, so two processes can end up writing to the
    same directory and corrupting it.

## Sizing

Budget for:

| | |
| --- | --- |
| Object payloads | Your data |
| Version history | Every non-current version of every object |
| Multipart parts | Parts of uploads not yet completed or aborted |
| Metadata | Grows with object count, not object size |
| Audit trail | Grows with request volume and is never pruned |

The gap between logical and physical bytes is version history plus multipart parts —
watch both:

```bash
record-store storage inspect --endpoint https://management.example.com
```

See [Capacity Planning](../operations/capacity-planning.md).

## Docker volumes

A named volume is the simplest correct choice:

```yaml
volumes:
  - record-store-data:/var/lib/record-store
```

A bind mount works too, but the host directory must be writable by uid `10001`:

```bash
sudo install -d -o 10001 -g 10001 /srv/record-store
```

```yaml
volumes:
  - /srv/record-store:/var/lib/record-store
```

Do not mount the data directory into two containers. The lock file will refuse the
second, which is the correct outcome but not one you want to discover in production.

## Backups

A filesystem snapshot of a running deployment can catch metadata mid-write. Use the
built-in backup, which takes the data lock and records a checksum per file:

```bash
record-store server backup-metadata --output /backups/2026-08-29
```

That covers `metadata/`. Back up `objects/` with your normal file backup — payloads
are immutable once committed, so an incremental copy is safe.

The command refuses to write to a directory that already exists, so each run needs a
fresh destination.

Restoring requires an **empty** `metadata/` directory, verifies every checksum, and
refuses a backup from an incompatible format or a newer schema:

```bash
record-store server restore-metadata /backups/2026-08-29
```

Both commands take the exclusive data lock, so the server must be stopped.

See [Backup and Restore](../operations/backup-and-restore.md).

## Separating temporary storage

```toml
[storage]
data_directory = "/var/lib/record-store"
temporary_directory = "/var/lib/record-store/tmp"
```

Only worth setting to move `tmp/` somewhere with different characteristics — and only
on the same filesystem as `objects/`, for the rename reason above.

## What must never be in the data directory

The credential master key. It is what makes the sealed contents readable; storing it
alongside them means one stolen backup is a complete compromise. Keep it in a secret
manager, and back it up separately.
