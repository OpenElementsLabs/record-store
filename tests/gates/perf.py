#!/usr/bin/env python3
"""PERF-BASELINE and PERF-ENDURANCE: reproducible operating measurements.

Record Store documents no latency, throughput or memory requirement, so this
gate does not invent one. It separates three kinds of threshold:

  1. Structural invariants -- blocking. They follow from documented design, not
     from a number anyone chose:
       * memory does not grow with object size (payloads stream to a temporary
         file, docs/concepts/durability.md): peak RSS growth while uploading and
         downloading a 256 MiB object is within 64 MiB of the growth for 32 MiB;
       * listing cost does not grow with position (ListObjectsV2 continues from
         a token): the slowest late page is within 3x the median early page;
       * zero errors in every workload that is not deliberately overloaded.
  2. Regressions -- blocking. The candidate and the previous release (the real
     binaries, --previous-bin-dir) run the same workload in alternating rounds
     on the same host, and each compared metric's median must be no worse than
     the previous release's by more than max(15 %, 3 x the larger coefficient
     of variation). A metric whose CV exceeds 25 % is too noisy to judge, and a
     run where most are is an invalid measurement, not a pass. Comparing on one
     host is what makes this meaningful on GitHub-hosted runners, whose CPUs
     differ from run to run.
  3. Absolute latency/throughput targets -- none. None is documented, so the
     medians are recorded for calibration rather than judged against a number
     chosen here.

Endurance (`--profile endurance`) runs the mixed workload for a fixed duration
and fits RSS and open files against time over the second half of the run.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import os
import random
import statistics
import threading
import time
from pathlib import Path

from gatelib import (
    Artifact,
    Gate,
    InvalidMeasurement,
    Secrets,
    Server,
    artifact_from_arguments,
    environment_fingerprint,
    filesystem_of,
    identify_artifact,
    read_all,
    sha256_bytes,
)

MIB = 1024 * 1024
MAX_CV = 0.25
MIN_TOLERANCE = 0.15

# Compared against the previous release, run in alternating rounds on the same
# host. Tail percentiles of the small-object runs and memory deltas are too
# noisy at these sample sizes to judge a regression on; they are recorded.
COMPARED = (
    "small_put_p50_seconds", "small_get_p50_seconds", "small_ops_per_second",
    "large_32mib_put_mib_s", "large_32mib_get_mib_s", "large_256mib_put_mib_s", "large_256mib_get_mib_s",
    "mixed_p50_seconds", "mixed_p99_seconds", "mixed_ops_per_second",
)

# Which direction is worse, per metric suffix.
HIGHER_IS_WORSE = ("_seconds", "_ms", "_bytes", "_ratio_error")
LOWER_IS_WORSE = ("_per_second", "_mib_s")


def percentile(values: list[float], fraction: float) -> float:
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int(round(fraction * (len(ordered) - 1))))]


class Sampler:
    """Samples the server process every interval while a workload runs."""

    def __init__(self, server: Server, interval: float = 0.1, track_storage: bool = False) -> None:
        self.server, self.interval, self.track_storage = server, interval, track_storage
        self.samples: list[dict] = []
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._run, daemon=True)

    def _run(self) -> None:
        while not self._stop.is_set():
            try:
                sample = {"t": time.monotonic(), **self.server.sample()}
                if self.track_storage:
                    # Beside RSS, so a curve can show whether memory follows the
                    # size of the database files (a page cache) or not (a leak).
                    sample["redb_bytes"] = float(sum(p.stat().st_size for p in self.server.data_directory.rglob("*.redb")))
                self.samples.append(sample)
            except Exception:  # noqa: BLE001
                return
            self._stop.wait(self.interval)

    def __enter__(self) -> "Sampler":
        self._thread.start()
        return self

    def __exit__(self, *_: object) -> None:
        self._stop.set()
        self._thread.join()

    def peak(self, field: str) -> float:
        return max(sample[field] for sample in self.samples) if self.samples else float("nan")


def small_objects(server: Server, rng: random.Random, count: int, concurrency: int) -> dict[str, float]:
    client = server.s3(max_pool_connections=concurrency * 2)
    body = rng.randbytes(4096)
    errors = 0
    put_latency, get_latency = [], []
    lock = threading.Lock()
    prefix = f"small-{rng.randrange(1 << 30)}"

    def one(index: int) -> None:
        nonlocal errors
        key = f"{prefix}/{index:06d}"
        try:
            started = time.perf_counter()
            client.put_object(Bucket="perf", Key=key, Body=body)
            middle = time.perf_counter()
            data = read_all(client, "perf", key)
            finished = time.perf_counter()
            if data != body:
                raise ValueError("wrong bytes")
            with lock:
                put_latency.append(middle - started)
                get_latency.append(finished - middle)
        except Exception:  # noqa: BLE001
            with lock:
                errors += 1

    started = time.perf_counter()
    with concurrent.futures.ThreadPoolExecutor(concurrency) as pool:
        list(pool.map(one, range(count)))
    elapsed = time.perf_counter() - started
    return {
        "small_put_p50_seconds": percentile(put_latency, 0.5),
        "small_put_p99_seconds": percentile(put_latency, 0.99),
        "small_get_p50_seconds": percentile(get_latency, 0.5),
        "small_get_p99_seconds": percentile(get_latency, 0.99),
        "small_ops_per_second": 2 * count / elapsed,
        "small_errors": float(errors),
    }


def large_object(server: Server, rng: random.Random, size: int, label: str) -> dict[str, float]:
    client = server.s3()
    # randbytes() is limited to 2**31 bits per call; build large payloads from
    # seeded 1 MiB chunks instead.
    body = b"".join(rng.randbytes(min(MIB, size - offset)) for offset in range(0, size, MIB))
    time.sleep(0.5)
    idle = server.sample()["rss_bytes"]
    with Sampler(server) as sampler:
        started = time.perf_counter()
        client.put_object(Bucket="perf", Key=f"large-{label}", Body=body)
        uploaded = time.perf_counter()
        data = read_all(client, "perf", f"large-{label}")
        downloaded = time.perf_counter()
    if sha256_bytes(data) != sha256_bytes(body):
        raise InvalidMeasurement(f"{label}: the large object did not round-trip")
    return {
        f"large_{label}_put_mib_s": size / MIB / (uploaded - started),
        f"large_{label}_get_mib_s": size / MIB / (downloaded - uploaded),
        f"large_{label}_peak_rss_growth_bytes": sampler.peak("rss_bytes") - idle,
    }


def mixed(server: Server, rng: random.Random, seconds: float, concurrency: int) -> tuple[dict[str, float], list[dict]]:
    client = server.s3(max_pool_connections=concurrency * 2)
    keys = [f"mixed-seed/{index:04d}" for index in range(200)]
    for key in keys:
        client.put_object(Bucket="perf", Key=key, Body=rng.randbytes(rng.choice((1024, 32 * 1024, 256 * 1024))))
    latencies: list[float] = []
    errors = 0
    lock = threading.Lock()
    stop = time.monotonic() + seconds

    def worker(number: int) -> None:
        nonlocal errors
        local = random.Random(number)
        while time.monotonic() < stop:
            started = time.perf_counter()
            try:
                if local.random() < 0.7:
                    read_all(client, "perf", local.choice(keys))
                else:
                    client.put_object(Bucket="perf", Key=local.choice(keys),
                                      Body=local.randbytes(local.choice((1024, 32 * 1024, 256 * 1024))))
                with lock:
                    latencies.append(time.perf_counter() - started)
            except Exception:  # noqa: BLE001
                with lock:
                    errors += 1

    with Sampler(server, interval=1.0, track_storage=seconds > 120) as sampler:
        started = time.perf_counter()
        with concurrent.futures.ThreadPoolExecutor(concurrency) as pool:
            list(pool.map(worker, range(concurrency)))
        elapsed = time.perf_counter() - started
    return {
        "mixed_p50_seconds": percentile(latencies, 0.5),
        "mixed_p99_seconds": percentile(latencies, 0.99),
        "mixed_ops_per_second": len(latencies) / elapsed,
        "mixed_errors": float(errors),
        "mixed_peak_rss_bytes": sampler.peak("rss_bytes"),
        "mixed_peak_open_files": sampler.peak("open_files"),
    }, sampler.samples


def catalog(server: Server, count: int) -> dict[str, float]:
    client = server.s3(max_pool_connections=64)
    client.create_bucket(Bucket="perf-catalog")
    with concurrent.futures.ThreadPoolExecutor(32) as pool:
        list(pool.map(lambda i: client.put_object(Bucket="perf-catalog", Key=f"k/{i:07d}", Body=b""), range(count)))
    page_times: list[float] = []
    listed = 0
    token = None
    started = time.perf_counter()
    while True:
        arguments = {"Bucket": "perf-catalog", "MaxKeys": 1000}
        if token:
            arguments["ContinuationToken"] = token
        page_started = time.perf_counter()
        page = client.list_objects_v2(**arguments)
        page_times.append(time.perf_counter() - page_started)
        listed += len(page.get("Contents", []))
        if not page.get("IsTruncated"):
            break
        token = page["NextContinuationToken"]
    walk = time.perf_counter() - started
    if listed != count:
        raise InvalidMeasurement(f"listing returned {listed} of {count} keys")
    early = statistics.median(page_times[: max(1, len(page_times) // 4)])
    late = max(page_times[-max(1, len(page_times) // 4):])
    audit_started = time.perf_counter()
    server.api("GET", "/api/v1/audit/events?operation=PutObject&limit=1000")
    audit_seconds = time.perf_counter() - audit_started
    return {
        "catalog_objects": float(count),
        "catalog_list_walk_seconds": walk,
        "catalog_page_p50_seconds": statistics.median(page_times),
        "catalog_late_to_early_page_ratio": late / early,
        "audit_filtered_query_seconds": audit_seconds,
    }


def environment_key(fingerprint: dict) -> str:
    cpu = (fingerprint.get("cpu_model") or "unknown").replace(" ", "-")
    return f"{fingerprint['os'].lower()}-{fingerprint['machine']}-{fingerprint['cpu_count']}cpu-{cpu}".lower()


def summarize(runs: list[dict[str, float]]) -> dict[str, dict[str, float]]:
    summary = {}
    for name in runs[0]:
        values = [run[name] for run in runs if name in run]
        median = statistics.median(values)
        spread = statistics.pstdev(values) / median if len(values) > 1 and median else 0.0
        summary[name] = {"median": median, "cv": spread, "runs": values}
    return summary


def worse_by(name: str, candidate: float, baseline: float) -> float:
    if baseline == 0:
        return 0.0
    if name.endswith(LOWER_IS_WORSE):
        return (baseline - candidate) / baseline
    return (candidate - baseline) / baseline


def main(gate: Gate) -> None:
    artifact, options = artifact_from_arguments()
    identify_artifact(gate, artifact)
    profile = options["profile"]
    repeats = {"pr": 3, "scheduled": 5, "endurance": 1}.get(profile, 3)
    # The weekly scheduled run carries the full workload. A candidate runs the
    # same shape smaller, so the whole CI run stays within 15 minutes: twelve
    # alternating runs of 60 s mixed load and two 50k catalogs took 20 minutes
    # on their own. Fewer samples widen the CV, which widens the tolerance, so a
    # smaller run is more lenient rather than flakier.
    small_count = {"pr": 400, "candidate": 1000}.get(profile, 2000)
    catalog_count = {"pr": 5000, "candidate": 20000}.get(profile, 50000)
    mixed_seconds = {"pr": 10.0, "candidate": 15.0}.get(profile, 60.0)
    fingerprint = environment_fingerprint()
    key = environment_key(fingerprint)
    gate.context.update(profile=profile, repeats=repeats, environment_key=key, fingerprint=fingerprint,
                        workload={"small_object_bytes": 4096, "small_ops_per_run": small_count * 2,
                                  "small_concurrency": 8, "large_sizes_mib": [32, 256],
                                  "mixed_read_ratio": 0.7, "mixed_sizes": [1024, 32768, 262144],
                                  "mixed_concurrency": 16, "mixed_seconds": mixed_seconds,
                                  "catalog_objects": catalog_count, "catalog_page_size": 1000,
                                  "warmup": "one discarded small-object pass per mode",
                                  "cache": "page cache warm; no drop between runs (not privileged)"})

    # The reference is the previous release, measured on the same host in the
    # same job: GitHub-hosted runners are not one machine, so a baseline recorded
    # on one CPU would rarely match the next run's, and hardware drift would be
    # read as regression. Rounds alternate which binary goes first.
    reference = Artifact(Path(options["previous_bin_dir"]).resolve()) if options["previous_bin_dir"] else None
    if reference is not None:
        gate.context["reference_artifact"] = reference.describe()
    runs_by_mode: dict[str, list[dict[str, float]]] = {}
    reference_by_mode: dict[str, list[dict[str, float]]] = {}
    endurance_samples: list[dict] = []
    for encrypted in (False, True):
        mode = "encrypted" if encrypted else "plaintext"
        data = gate.work_directory / mode / "data"
        server = Server(artifact, data, gate.evidence_directory / "logs", credentials=Secrets(),
                        encrypted=encrypted, name=f"perf-{mode}")
        cold_start = server.start()
        gate.context[f"{mode}_filesystem"] = filesystem_of(data)
        server.s3().create_bucket(Bucket="perf")
        rng = random.Random(f"{options['seed']}-{mode}")
        small_objects(server, rng, 100, 8)  # warm-up, discarded
        if profile == "endurance":
            duration = float(os.environ.get("GATE_ENDURANCE_SECONDS", "1800"))
            result, samples = mixed(server, rng, duration, 16)
            endurance_samples = samples
            runs_by_mode[mode] = [result]
            server.stop()
            break
        server.stop()
        contenders = [("candidate", server, rng, [])]
        if reference is not None:
            baseline = Server(reference, gate.work_directory / mode / "reference-data", gate.evidence_directory / "logs",
                              credentials=Secrets(), encrypted=encrypted, name=f"perf-{mode}-reference")
            baseline.start()
            baseline.s3().create_bucket(Bucket="perf")
            baseline_rng = random.Random(f"{options['seed']}-{mode}")
            small_objects(baseline, baseline_rng, 100, 8)  # warm-up, discarded
            baseline.stop()
            contenders.append(("reference", baseline, baseline_rng, []))
        for round_index in range(repeats):
            order = contenders if round_index % 2 == 0 else list(reversed(contenders))
            for _, contender, contender_rng, runs in order:
                contender.start()
                run: dict[str, float] = {}
                run.update(small_objects(contender, contender_rng, small_count, 8))
                for size, label in ((32 * MIB, "32mib"), (256 * MIB, "256mib")):
                    run.update(large_object(contender, contender_rng, size, label))
                result, _ = mixed(contender, contender_rng, mixed_seconds, 16)
                run.update(result)
                runs.append(run)
                contender.stop()
        runs = contenders[0][3]
        server.start()
        catalog_result = catalog(server, catalog_count)
        stop_seconds = server.stop()
        warm_start = server.start()
        server.stop()
        for run in runs:
            run.update(catalog_result)
            run.update({"startup_empty_seconds": cold_start, "startup_with_catalog_seconds": warm_start,
                        "shutdown_seconds": stop_seconds})
        runs_by_mode[mode] = runs
        if reference is not None:
            reference_by_mode[mode] = contenders[1][3]

    summary = {mode: summarize(runs) for mode, runs in runs_by_mode.items()}
    gate.context["summary"] = summary

    if profile == "endurance":
        t_start = endurance_samples[0]["t"] if endurance_samples else 0.0
        (gate.evidence_directory / "endurance-samples.json").write_text(json.dumps(
            [{**sample, "t": round(sample["t"] - t_start, 2)} for sample in endurance_samples]))
        if endurance_samples:
            first, last = endurance_samples[0], endurance_samples[-1]
            gate.context["endurance_start_end"] = {
                "rss_bytes": [first["rss_bytes"], last["rss_bytes"]],
                "redb_bytes": [first.get("redb_bytes"), last.get("redb_bytes")],
                "duration_seconds": round(last["t"] - first["t"], 1),
            }
        half = endurance_samples[len(endurance_samples) // 2:]
        # A slope fitted over minutes extrapolates noise into "growth per hour":
        # a few descriptors opening late in a 150 s run read as 50 files/hour.
        if len(half) < 10 or half[-1]["t"] - half[0]["t"] < 600:
            raise InvalidMeasurement("the second half of the endurance run spans less than 10 minutes; "
                                     "too short to fit a trend")
        t0 = half[0]["t"]
        xs = [(s["t"] - t0) / 3600 for s in half]
        for field, unit, limit in (("rss_bytes", "bytes/hour", 64 * MIB), ("open_files", "files/hour", 8.0)):
            ys = [s[field] for s in half]
            mean_x, mean_y = statistics.fmean(xs), statistics.fmean(ys)
            slope = sum((x - mean_x) * (y - mean_y) for x, y in zip(xs, ys)) / sum((x - mean_x) ** 2 for x in xs)
            gate.metric(f"endurance_{field}_slope", slope, unit, limit=limit, provisional=True,
                        basis="provisional: second-half growth must be near zero; calibrate from repeated runs")
        errors = summary["plaintext"]["mixed_errors"]["median"]
        gate.metric("endurance_errors", errors, "errors", limit=0, basis="a steady workload produces no errors")
        return

    for mode, metrics in summary.items():
        noisy = [name for name, value in metrics.items() if value["cv"] > MAX_CV and not name.endswith("_errors")]
        gate.context[f"{mode}_noisy_metrics"] = noisy
        for name in ("small_errors", "mixed_errors"):
            gate.metric(f"{mode}_{name}", metrics[name]["median"], "errors", limit=0,
                        basis="a workload within capacity produces no errors")
        small_growth = metrics["large_32mib_peak_rss_growth_bytes"]["median"]
        big_growth = metrics["large_256mib_peak_rss_growth_bytes"]["median"]
        gate.metric(f"{mode}_rss_growth_256mib_minus_32mib_bytes", big_growth - small_growth, "bytes", limit=64 * MIB,
                    basis="payloads stream (docs/concepts/durability.md): memory must not scale with object size")
        gate.metric(f"{mode}_catalog_late_to_early_page_ratio",
                    metrics["catalog_late_to_early_page_ratio"]["median"], "ratio", limit=3.0,
                    basis="continuation-token paging: cost must not grow with position in the listing")
        gate.metric(f"{mode}_startup_with_catalog_seconds", metrics["startup_with_catalog_seconds"]["median"], "s",
                    limit=5.0, basis="container start period in deploy/docker/Dockerfile HEALTHCHECK is 5 s")
        # No absolute latency or throughput target is set: none is documented,
        # and a number chosen here would read as a requirement. Medians are
        # recorded for calibration and compared against the baseline below.
        for name in ("small_get_p99_seconds", "small_put_p99_seconds", "large_256mib_put_mib_s",
                     "large_256mib_get_mib_s", "mixed_p99_seconds", "mixed_ops_per_second"):
            gate.metric(f"{mode}_{name}", metrics[name]["median"], "MiB/s" if name.endswith("_mib_s") else
                        ("ops/s" if name.endswith("_per_second") else "s"))

    if reference is None:
        gate.context["regression"] = "not evaluated: no reference release"
        raise InvalidMeasurement("no reference release binaries (--previous-bin-dir); regression cannot be judged")
    reference_summary = {mode: summarize(runs) for mode, runs in reference_by_mode.items()}
    gate.context["reference_summary"] = reference_summary
    comparisons: dict[str, object] = {}
    for mode, metrics in summary.items():
        for name in COMPARED:
            value, reference_value = metrics[name], reference_summary[mode][name]
            if value["cv"] > MAX_CV or reference_value["cv"] > MAX_CV:
                comparisons[f"{mode}_{name}"] = "invalid: too noisy"
                continue
            tolerance = max(MIN_TOLERANCE, 3 * max(value["cv"], reference_value["cv"]))
            regression = worse_by(name, value["median"], reference_value["median"])
            comparisons[f"{mode}_{name}"] = {"candidate": value["median"], "reference": reference_value["median"],
                                              "worse_by": regression, "tolerance": tolerance}
            gate.check(f"no regression against the previous release: {mode} {name} "
                       f"({regression:+.1%} vs tolerance {tolerance:.0%})", regression <= tolerance)
    gate.context["regression"] = comparisons
    invalid = [name for name, result in comparisons.items() if result == "invalid: too noisy"]
    if invalid and len(invalid) * 2 > len(comparisons):
        raise InvalidMeasurement(f"most comparisons are too noisy to judge: {invalid[:10]}")


if __name__ == "__main__":
    Gate(os.environ.get("GATE_ID", "PERF-BASELINE"), "operating measurements against structural limits and the previous release").run(main)
