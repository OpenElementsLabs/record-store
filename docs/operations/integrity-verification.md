# Integrity Verification

Every object is stored with a SHA-256 checksum recorded at write time. Verification
reads the bytes back and compares.

## Verifying one object

```bash
record-store verify object uploads invoices/2026/03/inv-1.pdf \
  --endpoint https://management.example.com
```

Reads the payload — decrypting it if encryption is enabled — and checks it against the
stored checksum. Success returns the object's metadata; a mismatch is an error.

## Verifying a bucket

```bash
record-store verify bucket uploads --endpoint https://management.example.com
```

Walks every object in the bucket and reports:

| Field | Meaning |
| --- | --- |
| `verified_objects` | Objects checked |
| `failed_objects` | Objects whose bytes no longer match |

This reads every byte in the bucket. On a large bucket it is slow and I/O-heavy — run
it off-peak.

## Storage inspection

A cheaper, structural check: does metadata match what is on disk?

```bash
record-store storage inspect --endpoint https://management.example.com
```

| Field | Meaning |
| --- | --- |
| `metadata_payloads_scanned` | Payload records examined |
| `data_payloads_scanned` | Payload files examined |
| `metadata_without_data` | **Metadata referencing a missing file** |
| `data_without_metadata` | Orphaned payload files |
| `unknown_data_entries` | Files in the object store that are not recognised |
| `recognized_temporary_entries` | Expected files under `tmp/` |
| `unknown_temporary_entries` | Unrecognised files under `tmp/` |
| `truncated` | The scan hit its entry limit |
| `missing_payload_samples` | Example object IDs with missing data |
| `orphan_payload_samples` | Example orphaned payloads |

Two rows matter most:

- **`metadata_without_data` is data loss.** An object exists as far as clients are
  concerned and its bytes are gone.
- **`data_without_metadata` is wasted space.** Nothing references those files, usually
  the residue of an interrupted delete.

`truncated: true` means you saw a partial picture. Raise the bound:

```bash
record-store storage inspect --maximum-entries 1000000 \
  --endpoint https://management.example.com
```

Default is 100000 entries.

## Storage repair

```bash
# Dry run — reports what would be removed and removes nothing
record-store storage repair --endpoint https://management.example.com

# Apply
record-store storage repair --apply --endpoint https://management.example.com
```

Repair is **dry-run by default**. Without `--apply` it inspects and reports.

What it does:

- **Removes orphaned payloads** — files no metadata references.
- **Never removes unknown files.** A file it does not recognise is reported, not
  deleted. An unrecognised file might be someone else's, or evidence.

What it cannot do: recover a missing payload. `metadata_without_data` is not repairable
from within the deployment — the bytes are gone, and they come back from a
[backup](backup-and-restore.md) or not at all.

Always dry-run first and read the numbers.

## A verification routine

| Frequency | Action |
| --- | --- |
| Daily | `storage inspect` — cheap, structural |
| Weekly | `verify bucket` on critical buckets, off-peak |
| After any incident | `storage inspect` first, then `verify bucket` on anything suspect |
| Before a major upgrade | `storage inspect` |

```bash
#!/usr/bin/env bash
set -euo pipefail

result=$(record-store --json storage inspect \
  --endpoint https://management.example.com)

missing=$(echo "$result" | jq '.metadata_without_data')
if [ "$missing" -gt 0 ]; then
  echo "ALERT: $missing objects have missing payloads"
  exit 1
fi
```

## When verification fails

1. **Do not repair.** `storage repair` does not recover missing data, and you want the
   evidence intact.
2. **Establish the scope** with `storage inspect` and `verify bucket`.
3. **Check the hardware.** Checksum mismatches usually mean a failing disk or bad
   memory — look at SMART data and the kernel log.
4. **Restore the affected objects from backup** — see
   [Backup and Restore](backup-and-restore.md).
5. **Then** consider `storage repair --apply` to clean up orphans.

A checksum mismatch is a hardware signal before it is a Record Store problem. Find out
why the bytes changed before deciding what to do about them.
