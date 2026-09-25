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

## Measured on the reference environment (2026-09-23)

PERF-BASELINE, candidate profile, GitHub-hosted `ubuntu-24.04` (AMD EPYC 7763,
4 vCPU, ext4), medians of five runs, client on the same host:

| Metric | Plaintext | Encrypted |
| --- | --- | --- |
| 4 KiB PUT p50 | 5.4 ms | 5.7 ms |
| 4 KiB GET p50 | 42.6 ms | 42.7 ms |
| 4 KiB PUT+GET throughput, 8 clients | 328 ops/s | 326 ops/s |
| 256 MiB PUT | 191 MiB/s | 165 MiB/s |
| 256 MiB GET | **98 MiB/s** | 466 MiB/s |
| Mixed 70/30, 16 clients | 450 ops/s, p99 61 ms | 475 ops/s, p99 62 ms |
| List 50 000 keys at 1000/page | 4.9 s | 5.1 s |

Small writes take ~5 ms on Linux against ~156 ms on the developer Mac, so most
of the effect above is macOS `F_FULLFSYNC`, not the product; whether writes
still serialize at higher concurrency on Linux is not yet measured.

Two further observations, neither judged against a requirement: a 4 KiB GET is
slower than a 4 KiB PUT (42 ms against 5 ms), and a whole-object plaintext read
of a large object is about five times slower than an encrypted one (the same
ratio appears on the Mac: 327 against 950 MiB/s). Both are worth a look before
anyone writes a throughput target.
