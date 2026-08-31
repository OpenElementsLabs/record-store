# Backup and Restore

A Record Store deployment has two things worth backing up, plus one thing that is not
in either.

| | Where | How |
| --- | --- | --- |
| Metadata | `<data_directory>/metadata/` | `record-store server backup-metadata` |
| Object payloads | `<data_directory>/objects/` | Your normal file backup |
| **The credential master key** | Your secret manager | Manually, once |

!!! danger "The master key is not in any backup"
    `RECORD_STORE_CREDENTIAL_MASTER_KEY` seals credentials, webhook secrets, and — with
    encryption on — every object. A backup without it is unusable.

    It is deliberately not stored in the data directory: keeping it there would make one
    stolen backup a complete compromise. Back it up separately, once, and record where
    it lives in your runbook.

## Metadata and payloads are one unit

Object payloads are meaningless without the catalog that names them, and catalog
entries pointing at absent payloads are data loss.

Back them up together, and restore them together, from the **same point in time**.
Mixing a Tuesday catalog with a Thursday payload directory produces both dangling
references and orphans.

## Backing up metadata

```bash
record-store server backup-metadata --output /backups/2026-08-29
```

What it does:

- Takes the exclusive data lock, so nothing writes while it runs
- Copies every `.redb` file from `metadata/`
- Records a SHA-256 checksum and size per file in `manifest.json`
- Stamps the backup format version and the metadata schema version

The output directory must not already exist. Each run needs a fresh destination — that
is what stops a partial overwrite of a good backup.

!!! warning "The server must be stopped"
    The command takes the exclusive data lock. It fails with a "data directory in use"
    error if the server is running.

    A filesystem snapshot of a *running* deployment can catch metadata mid-write. Use
    this command rather than trusting a live snapshot.

## Backing up payloads

Object payloads are immutable once committed, so an incremental copy is safe:

```bash
rsync -a --delete \
  /var/lib/record-store/objects/ \
  /backups/2026-08-29/objects/
```

Do not back up `tmp/` — it holds incomplete uploads and is disposable.

## A backup script

```bash
#!/usr/bin/env bash
set -euo pipefail

STAMP=$(date +%Y-%m-%d)
DEST="/backups/${STAMP}"
DATA="/var/lib/record-store"

docker stop --time 40 record-store

record-store server backup-metadata \
  --config /etc/record-store/config.toml \
  --output "${DEST}/metadata"

rsync -a "${DATA}/objects/" "${DEST}/objects/"

docker start record-store

echo "backup complete: ${DEST}"
```

The downtime is the length of the metadata copy plus the rsync delta. For a deployment
that cannot take that, snapshot the whole volume with a filesystem that does atomic
snapshots (ZFS, LVM) and take the metadata backup from the snapshot.

## Restoring metadata

```bash
# 1. Stop the server
docker stop --time 40 record-store

# 2. The metadata directory must be empty
mv /var/lib/record-store/metadata /var/lib/record-store/metadata.old

# 3. Restore
record-store server restore-metadata /backups/2026-08-29/metadata

# 4. Restore payloads from the same backup
rsync -a /backups/2026-08-29/objects/ /var/lib/record-store/objects/

# 5. Start
docker start record-store

# 6. Verify
record-store storage inspect --endpoint http://127.0.0.1:7601
```

Restore refuses to proceed unless:

- The `metadata/` directory is empty
- Every file's size and SHA-256 match the manifest
- The backup format version matches
- The metadata schema version is not **newer** than the running binary

Move the old directory aside rather than deleting it. If the restore turns out to be
the wrong one, you still have what you had.

Files are staged into a temporary directory and renamed into place only after every
checksum verifies, so a failed restore leaves nothing half-applied.

## Testing restores

An untested backup is a belief, not a backup.

```bash
# On a scratch machine, into an empty data directory
RECORD_STORE_STORAGE_DATA_DIRECTORY=/tmp/restore-test \
RECORD_STORE_ROOT_ACCESS_KEY=<your-access-key> \
RECORD_STORE_ROOT_SECRET_KEY=<your-secret-key> \
RECORD_STORE_CREDENTIAL_MASTER_KEY=<the same master key> \
  record-store server restore-metadata /backups/2026-08-29/metadata

rsync -a /backups/2026-08-29/objects/ /tmp/restore-test/objects/

# Start it and confirm the data is really there
record-store storage inspect --endpoint http://127.0.0.1:7601
record-store verify bucket uploads --endpoint http://127.0.0.1:7601
```

Use the **same master key**. A restore with a different one produces a deployment whose
credentials are unreadable — which is exactly the failure a test should catch.

Test quarterly, and after any upgrade that changed the metadata schema.

## Retention

A workable schedule:

| Frequency | Keep |
| --- | --- |
| Daily | 7 days |
| Weekly | 4 weeks |
| Monthly | 12 months |

Store at least one copy off the machine that runs Record Store, and encrypt or
access-control the backup location — it contains everything.

The audit trail is inside the metadata backup and is never pruned, so backups grow with
request volume as well as with data. See [Capacity Planning](capacity-planning.md).
