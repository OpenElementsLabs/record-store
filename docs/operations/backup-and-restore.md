# Backup and Restore

One command backs up a Record Store deployment, and one restores it. The thing that
is *not* in the backup — the credential master key — is the part that needs your
attention, so it is dealt with first.

!!! danger "The master key is not in any backup"
    `RECORD_STORE_CREDENTIAL_MASTER_KEY` seals service-account credentials, webhook
    secrets, share capabilities, and — with encryption on — every object payload. A
    backup without it is unusable.

    It is deliberately not stored in the data directory: keeping it there would make
    one stolen backup a complete compromise. Back it up separately, once, and record
    where it lives in your runbook.

    Root credentials and management tokens are not in the backup either. They come
    from the environment on every start, and a restored deployment expects the same
    ones.

## Taking a backup

```bash
docker stop --time 40 record-store

record-store server backup /backups/2026-09-22
```

```text
backup complete: /backups/2026-09-22
  metadata           6 files         4919296 bytes
  system             1 files              28 bytes
  objects           11 files      2348810258 bytes
  consistency  offline-exclusive-lock
  secrets      not included; recover key material separately
```

The destination holds everything a deployment needs:

| Component | What is in it | Required |
| --- | --- | --- |
| `metadata/` | Catalog, credentials, audit, events, sharing, lifecycle | Yes |
| `system/` | Storage format, and which master key the payloads use | Yes |
| `objects/` | Every committed payload | Yes, but may be empty |
| `backup-manifest.json` | Format versions, component inventory, sizes, a SHA-256 per file | Yes |

## What "consistent" means here

`consistency: offline-exclusive-lock` is a literal statement, not a reassurance.

The command takes the data directory's exclusive lock — the same lock the server
takes — and holds it for the whole copy. So the server cannot be running, nothing is
writing, and the result is a single point in time. If the server *is* running, the
command refuses:

```text
error: the data directory is in use by another Record Store process; stop the server first
```

There is no online mode. Record Store does not claim one, because a copy taken while
the catalog is being written can catch it mid-transaction, and a backup that is only
*usually* consistent is worse than a short outage.

**If you cannot take the outage**, snapshot the whole volume with a filesystem that
does atomic snapshots (ZFS, LVM), then run `record-store server backup` against the
snapshot. The lock and the guarantee then apply to the snapshot, which is what you
wanted.

## Incomplete backups cannot be mistaken for finished ones

While a backup is being written, the destination holds a file named `INCOMPLETE`. It
is removed only after every component is on disk and the manifest is published. So a
backup is complete if, and only if, `INCOMPLETE` is absent and `backup-manifest.json`
is present.

Verification and restore both refuse a directory that still has the marker:

```text
problem  the backup is marked INCOMPLETE: it was interrupted while being written
         and must not be restored
```

A destination holding a **completed** backup is never overwritten, with or without
flags. A destination holding an interrupted one can be reused with
`--replace-incomplete`.

Before it copies anything, the command compares the source size against the
destination's free space and refuses early if it will not fit. That check is
advisory: nothing stops another process from consuming the space while the copy runs.
A destination that fills mid-copy still fails safely — the backup is simply never
published.

## Checking a backup

An untested backup is a belief, not a backup.

```bash
record-store server verify-backup /backups/2026-09-22 --level full
```

There are three levels, and they are named separately because they prove very
different things:

| Level | What it proves | What it does not |
| --- | --- | --- |
| `manifest` | The manifest parses, the format is supported, the required components are listed, the backup is marked complete | Reads no file contents. A corrupted payload passes. |
| `checksums` (default) | Every listed file's size and SHA-256 recomputed from disk | Says nothing about whether the catalog and the payloads agree |
| `full` | Every payload the catalog references is present; every payload present is referenced | Does not read object *contents* against their stored checksums — that is `record-store verify bucket`, on a running deployment |

Do not describe a `manifest` check as verifying a backup. It has not read a byte of
it.

With encryption on, verification also confirms that the configured master key is the
one these payloads were written under. It compares a derived, public key reference;
the key itself is never read into the output.

```text
key      the supplied master key matches these payloads
```

Exit codes are stable, so a backup script can tell the cases apart:

| Code | Meaning |
| --- | --- |
| 0 | Success |
| 1 | Unexpected failure, such as an I/O error; the message says what failed |
| 2 | Configuration or arguments unusable, or a data directory holding an unfinished restore |
| 3 | The backup is damaged, incomplete, or missing a required component |
| 4 | The destination already holds something that must not be overwritten |
| 5 | Not enough space at the destination |
| 6 | A Record Store process still holds the data directory |
| 7 | `doctor` found failing checks |

Add `--json` to any of these commands for machine-readable output.

## Restoring

```bash
# 1. Stop anything running against the destination
docker stop --time 40 record-store

# 2. Restore into an empty data directory
record-store server restore /backups/2026-09-22 --level full

# 3. Start, with the same master key
docker start record-store

# 4. Confirm
record-store storage inspect --endpoint http://127.0.0.1:7601
record-store verify bucket uploads --endpoint http://127.0.0.1:7601
```

Restore verifies the whole backup **before writing anything**, then stages every file
into a temporary directory inside the data directory and renames the components into
place.

It refuses to touch a data directory that already holds `metadata/`, `objects/`, or
`system/`. A live deployment is never restored over, and two deployments are never
merged — a catalog from one and payloads from another produce dangling references in
one direction and orphans in the other.

### If a restore is interrupted

A restore writes a marker into the data directory and removes it only when every
component is in place. While the marker is there:

- the server **refuses to start**, so a half-restored deployment cannot come up
  looking ready;
- running the restore again clears what the interrupted attempt left behind and
  starts over, with no manual cleanup.

```text
Error: start-up checks failed: restore_state: a restore into this data directory was
interrupted and never finished — run `record-store server restore` again with the same
backup; the interrupted attempt is cleared automatically
```

### What a restore reports

```text
restored /backups/2026-09-22 into /var/lib/record-store
verified at level full
  metadata           6 files         4919296 bytes
  system             1 files              28 bytes
  objects           11 files      2348810258 bytes
  note: payloads are encrypted and the configured credential master key was confirmed
        to be the right one; it is not in the backup, so keep recovering it separately
  note: root credentials and management tokens come from the environment, not the backup
```

## A backup script

```bash
#!/usr/bin/env bash
set -euo pipefail

DEST="/backups/$(date +%Y-%m-%d)"

docker stop --time 40 record-store
record-store server --config /etc/record-store/config.toml backup "${DEST}"
docker start record-store

record-store server verify-backup "${DEST}" --level full --json > "${DEST}.verify.json"
```

The outage is the length of the copy. On an NVMe-backed deployment that is roughly a
second per two gigabytes; see [Capacity Planning](capacity-planning.md) for how to
estimate yours.

## The restore drill

Test onto a clean machine, or at least a clean directory, quarterly and after any
upgrade that changed the metadata schema.

```bash
RECORD_STORE_STORAGE_DATA_DIRECTORY=/srv/restore-drill \
RECORD_STORE_ROOT_ACCESS_KEY=<your access key> \
RECORD_STORE_ROOT_SECRET_KEY=<your secret key> \
RECORD_STORE_CREDENTIAL_MASTER_KEY=<the same master key> \
  record-store server restore /backups/2026-09-22 --level full
```

Then start it and confirm the deployment is really there, not just the files:

```bash
record-store storage inspect --endpoint http://127.0.0.1:7601
record-store verify bucket uploads --endpoint http://127.0.0.1:7601
```

Use the **same master key**. A restore with a different one is refused before
anything is written, which is exactly the failure a drill should catch. That holds
with payload encryption off too: the master key — or, where none is configured, the
root secret — also seals service-account secrets, share links and webhook secrets,
which would never unseal. The backup records a one-way reference to that key material
(`sealing_key_reference` in the manifest), never the material itself; backups taken
before this reference existed are restored without the check.

## Compatibility with older backups

`record-store server backup-metadata` and `restore-metadata` still work and still
behave as they did. They copy metadata only — no payloads, no system records — so
they warn on use and are no longer the documented path.

Backups those commands produced are still restorable by `record-store server
restore`, which reports them honestly as carrying one component:

```text
restored /backups/old into /var/lib/record-store
verified at level checksums
  metadata           6 files         4358144 bytes
```

You still have to bring the payloads yourself, from whatever copied them.

## Retention

| Frequency | Keep |
| --- | --- |
| Daily | 7 days |
| Weekly | 4 weeks |
| Monthly | 12 months |

Store at least one copy off the machine that runs Record Store, and encrypt or
access-control the backup location — it contains everything except the key.

The audit trail is inside the backup and is never pruned, so backups grow with request
volume as well as with data. See [Capacity Planning](capacity-planning.md).
