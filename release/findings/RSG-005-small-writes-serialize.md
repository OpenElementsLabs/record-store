# RSG-005: Small-write throughput does not scale with concurrency

| | |
| --- | --- |
| Status | open (observation; no requirement exists to judge it against) |
| Severity | unknown until measured on the reference environment |
| Blocks release | no |
| Gate | PERF-BASELINE (recorded medians) |
| Found | 2026-09-23, candidate `0765aee`, Apple M5 / macOS 26.6.2 / APFS |

4 KiB PUT throughput on the developer host, boto3 client on the same machine:

| Concurrency | PUT/s | GET/s |
| --- | --- | --- |
| 1 | 38.1 | 171.7 |
| 4 | 42.9 | 247.5 |
| 8 | 51.1 | 277.4 |
| 32 | 53.1 | 255.4 |

Writes appear to commit one at a time at ~20–26 ms each, which on APFS is
consistent with `F_FULLFSYNC` per commit. macOS is not a supported server
platform, so this is not the number users will see; it is the reason to measure
the reference environment before anyone writes a throughput target. Record the
first scheduled `PERF-BASELINE` on `ubuntu-24.04` and decide whether per-commit
serialization is intended.
