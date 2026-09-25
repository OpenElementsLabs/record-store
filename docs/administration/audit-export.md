# Audit Export

Two things an auditor asks for: a copy of the audit trail for a period, and a
statement of what is currently retained. Both are read-only, both are available
to the **auditor** role, and both work over the management API as well as the
CLI.

## Exporting a range

```bash
record-store audit-export export \
  --from 2026-01-01T00:00:00Z \
  --to   2026-02-01T00:00:00Z \
  --format json \
  --out ./audit-2026-01 \
  --endpoint https://management.example.com
```

The range is **`[from, to)`** — inclusive of the start, exclusive of the end.
That is what makes consecutive exports tile: January and February together
contain every record exactly once, with nothing duplicated at the boundary and
nothing lost.

`--format` is `json` or `csv`.

## What the directory contains

```text
audit-2026-01/
├── audit.json        the records
├── manifest.json     range, format, export id, who exported it
├── checkpoints.json  the checkpoint roots covering the range
└── SHA256SUMS        digests of the three files above
```

Check the copy with the tool you already have:

```bash
cd audit-2026-01 && sha256sum -c SHA256SUMS
```

## What an export proves, and what it does not

**`SHA256SUMS` establishes that the copy reached you unaltered.** It is computed
by the client as the bytes arrive, so it covers the transfer.

**It does not on its own establish that the log was not edited before the copy was
taken.** An export is a copy of what the server says the trail contains. Two further
things carry that argument, and they reach different distances:

- the **hash chain**, which the server maintains and which you can recheck with
  `record-store audit-export verify-chain`. It detects a record edited, removed, or
  reordered by anyone who could not also rewrite every later link.
- a **checkpoint**, and beyond it an **external anchor** over that checkpoint, which
  is what reaches an operator who rewrote the whole log and every hash in it. This
  release does not produce either.

`checkpoints.json` states which of these you have:

```json
{
  "status": "unavailable",
  "reason": "not_yet_checkpointed",
  "detail": "this deployment maintains a hash-chained audit log but does not yet
             produce checkpoints, so no Merkle root covers this range. …"
}
```

That file is always written. A missing section would read as "nothing to
report"; an explicit `unavailable` reads as "this was not established", and the
two are different claims.

!!! note "Checkpoints and anchoring are not implemented yet"
    Every export from this release reports `not_yet_checkpointed`. The field exists
    so exports taken now stay readable by tooling built later, and so nobody
    mistakes an unanchored copy for an anchored one. The chain underneath it is
    real; see
    [Audit Log](audit-log.md#checking-the-log-has-not-been-edited).

## Formats

**`json`** is a single JSON array, streamed. It parses with any JSON reader:

```bash
jq '[.[] | select(.result == "denied")] | length' audit-2026-01/audit.json
```

**`csv`** is RFC 4180 with a pinned column order:

```text
timestamp,event_id,principal,credential_id,source_ip,request_id,operation,resource,result,metadata
```

`metadata` holds a JSON object in one column. Flattening it into columns would
make the column set depend on the data, so two exports of the same deployment
could disagree about their own shape.

## Bounds

An export never loads the range into memory — records are paged from the store
and streamed to disk as they arrive, on both the server and the client. An
auditor asking for a year of a busy deployment gets a long download, not an
outage.

## Every export is recorded

Requesting an export writes an audit record naming who asked and for what:

```bash
record-store audit --limit 50 | grep audit.export
```

The record carries the export id, the range, and the format. It is written when
the export is **authorized**, not when the bytes finish — a transfer that fails
midway still represents a range being handed out, and that is the fact worth
keeping. If the record cannot be written, the export is refused rather than
performed untracked.

One consequence worth knowing: an export's own record lands in the audit trail,
so an export whose range includes the present will contain the record of itself.

## Retention report

Which buckets have Object Lock, what is currently held, and when each retention
expires:

```bash
record-store audit-export retention-report --endpoint https://management.example.com
```

```json
{
  "generated_at": "2026-09-20T07:06:07Z",
  "buckets": [
    { "bucket": "records", "default_retention": { "mode": "compliance",
                                                  "period": { "unit": "days", "value": 2555 } } }
  ],
  "versions": [
    { "bucket": "records", "key": "statement.pdf", "version_id": "…",
      "retention_mode": "COMPLIANCE", "retain_until": "2033-01-01T00:00:00Z",
      "legal_hold": false, "status": "held" }
  ],
  "held_count": 1,
  "truncated": false
}
```

`status` distinguishes two things a single list would blur:

| `status` | Meaning |
| --- | --- |
| `held` | A retention has not elapsed, a legal hold is on, or both |
| `elapsed` | A lock record exists, but nothing it describes still holds the version |

An elapsed entry is still listed. The record is still there, and an auditor
asking what is retained wants to see it rather than have it disappear — but it
is not counted in `held_count`, because it no longer protects anything.

`generated_at` matters: retention is relative to the current time, so a report
without its own timestamp cannot be interpreted later.

`truncated` is `true` when the scan stopped before the end of the lock table.
The report is bounded so it stays readable; truncation is reported rather than
silent, because a report that quietly stopped would understate what is retained.

The scan walks the Object Lock table, which holds only locked versions, so a
deployment with a million objects and ten locks pays for ten.

See [Object Lock](object-lock.md) for how retention is applied in the first
place.

## Roles

| Route | Auditor | Storage admin | System admin |
| --- | --- | --- | --- |
| `GET /api/v1/audit/export` | yes | yes | yes |
| `GET /api/v1/audit/export/manifest` | yes | yes | yes |
| `GET /api/v1/reports/retention` | yes | yes | yes |

The auditor role is read-only everywhere, so these are reachable with a token
that can change nothing. See [Audit Log](audit-log.md).
