# RSG-011: Resident memory grows steadily under a steady mixed workload

| | |
| --- | --- |
| Status | open (investigating; cause not yet confirmed) |
| Severity | unknown until the curve is measured — up to an out-of-memory kill on a small host |
| Blocks release | no — PERF-ENDURANCE is provisional and non-blocking; needs a maintainer decision |
| Gate | PERF-ENDURANCE |
| Found | 2026-09-23, candidate `417091a`, GitHub-hosted `ubuntu-24.04` (AMD EPYC 9V45, 4 vCPU) |

A 30-minute run of 16 clients over 200 keys (70 % reads, 1 KiB–256 KiB
objects, ~450 operations per second, zero errors) reached a peak RSS of
**968 MB**; the least-squares slope over the second half was **1.76 GB/hour**.
The process idles at 20–40 MB. Open descriptors stayed flat (−0.6/hour).

## Leading hypothesis, not yet confirmed

Every redb database is opened by `crates/record-store-core/src/redb_open.rs`
with redb's defaults, which include a **1 GiB page cache per database**, and a
deployment keeps several databases under `metadata/`. The audit trail and event
journal grow with every request, so the caches have ever more pages to hold.
If that is the cause, memory is bounded — but by roughly 1 GiB times the number
of databases, is not configurable, and is documented nowhere; a container with
a 1–2 GiB limit would be killed under sustained traffic.

What argues for caution: a three-minute run on the developer host (~14 000
operations) stayed flat at ~27 MB while the database files grew from 5 to
19 MB, so the growth is not a simple per-request leak at that scale — but that
run was ~60 times smaller than the CI one.

## Next step

PERF-ENDURANCE now saves every sample with the size of the `.redb` files beside
RSS (`endurance-samples.json`). The next weekly run shows whether RSS follows
the database size and plateaus (a cache) or keeps climbing (a leak). Then either
bound and document the caches (`Builder::set_cache_size`), or fix the leak, and
turn the endurance limit into a blocking one.
