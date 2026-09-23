# RSG-012: Reads under write load were twice as slow as in 0.1.3

| | |
| --- | --- |
| Status | fixed |
| Severity | medium — a performance regression against the previous release |
| Blocks release | no — fixed |
| Gate | PERF-BASELINE (same-host comparison with 0.1.3) |
| Found | 2026-09-23, candidate `8a57dea`, developer host; the Linux runs had shown small GETs at 42 ms against 5 ms for PUTs |

The first same-host comparison with 0.1.3 found small-object GET p50 up
+119 % (plaintext) and +139 % (encrypted), small PUT and mixed-workload latency
+16–28 %. Unloaded, the two versions were equal (4.0 ms against 4.1 ms): the
regression appeared only with concurrent writes.

| 4 KiB GET p50 while 8 clients write | GET | writes/s |
| --- | --- | --- |
| 0.1.3 | 17.6 ms | 58.0 |
| candidate before the fix | 36.0 ms | 50.7 |
| candidate with group commit | 19.2 ms | 53.6 |

## Cause

The candidate audits every change twice — an `attempted` record before it and
its outcome after (docs/administration/audit-log.md) — where 0.1.3 wrote one.
Each record was its own durable redb commit through the audit database's
single writer, and reads are audited too, so under write load a read's record
queued behind twice as many fsyncs as before.

## Fix

Group commit in `crates/record-store-audit`: one writer thread takes every
queued append, applies them in arrival order in one transaction — sequence
numbers and hash links assigned exactly as before — commits once, and only
then tells each caller its record is durable. A failure in a batch retries its
appends one by one, so one bad record fails nobody else. The guarantee is
unchanged: no caller proceeds before its record is on disk. Regression test:
`concurrent_appends_form_one_gapless_verifying_chain` (400 concurrent appends,
chain intact and complete after reopen). The remaining ~8 % write-throughput
gap is the second, deliberate audit record per change.
