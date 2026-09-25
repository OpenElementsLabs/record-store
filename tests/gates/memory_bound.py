#!/usr/bin/env python3
"""RES-MEMORY: resident memory stops growing with history once the caches are full.

docs/operations/capacity-planning.md#memory: the metadata page cache is bounded
by `storage.metadata_cache_mib`, and it is the only part of the server's memory
that grows with the size of its databases. Before it was bounded, every redb
database cached up to 1 GiB, so memory tracked the audit trail and event
journal -- about 1.3 KB of resident memory per request under the endurance
workload, until a container limit killed the process (release/findings/RSG-011,
closed).

The gate runs that workload against the real binary with a cache far smaller
than the data it writes, in the environment the container image ships
(`MALLOC_ARENA_MAX=2`), and after a warm-up in which the caches fill:

  * the audit trail and event journal outgrow the cache several times over;
  * resident memory grows by at most RSS_BYTES_PER_REQUEST_LIMIT per request --
    a limit a fifth of what an unbounded cache produced, and several times what
    a bounded one does, so neither runner speed nor noise decides it;
  * no request fails.

The measure is growth per request rather than per second or per byte of file,
so a slower runner simply makes fewer requests: it is judged on the same ratio.
redb grows its files in large steps, which is why file size is recorded rather
than divided by.
"""

from __future__ import annotations

import multiprocessing
import random
import statistics
import threading
import time

from gatelib import Gate, InvalidMeasurement, Secrets, Server, artifact_from_arguments, identify_artifact

CACHE_MIB = 8
# Long enough for the audit and event shares of the cache to fill even on a slow
# runner; what grows after it is what does not stop growing.
WARM_UP_SECONDS = 120
MEASURE_SECONDS = 180
PROCESSES = 4
THREADS = 4
KEYS = 200
READ_FRACTION = 0.7
# Measured on Linux: an unbounded cache 1189-1300 B/request; bounded, 50-160.
RSS_BYTES_PER_REQUEST_LIMIT = 256.0
# Below this many requests in the measured window the ratio is noise.
MINIMUM_REQUESTS = 15_000


def client_process(endpoint: str, access: str, secret: str, stop_at: float, counters, seed: int) -> None:
    import boto3
    from botocore.config import Config

    def run(index: int) -> None:
        rng = random.Random(seed * 1000 + index)
        client = boto3.client(
            "s3", endpoint_url=endpoint, region_name="us-east-1",
            aws_access_key_id=access, aws_secret_access_key=secret,
            config=Config(signature_version="s3v4", s3={"addressing_style": "path"},
                          request_checksum_calculation="when_required",
                          response_checksum_validation="when_required",
                          retries={"max_attempts": 1, "mode": "standard"}, max_pool_connections=2))
        done = failed = 0
        while time.time() < stop_at:
            key = f"k/{rng.randrange(KEYS):05d}"
            try:
                if rng.random() < READ_FRACTION:
                    try:
                        client.get_object(Bucket="memory", Key=key)["Body"].read()
                    except client.exceptions.NoSuchKey:
                        pass
                else:
                    client.put_object(Bucket="memory", Key=key, Body=rng.randbytes(rng.randint(1024, 256 * 1024)))
                done += 1
            except Exception:  # noqa: BLE001
                failed += 1
            if done % 25 == 0:
                with counters.get_lock():
                    counters[0] += 25
        with counters.get_lock():
            counters[0] += done % 25
            counters[1] += failed

    threads = [threading.Thread(target=run, args=(index,)) for index in range(THREADS)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()


def window(samples: list[dict], start: float, end: float, field: str) -> float:
    values = [sample[field] for sample in samples if start <= sample["t"] <= end]
    if not values:
        raise InvalidMeasurement(f"no samples between {start} s and {end} s")
    return statistics.median(values)


def main(gate: Gate) -> None:
    artifact, _ = artifact_from_arguments()
    identify_artifact(gate, artifact)
    server = Server(artifact, gate.work_directory / "data", gate.evidence_directory / "logs",
                    credentials=Secrets(), encrypted=False, name="memory",
                    extra_environment={"RECORD_STORE_STORAGE_METADATA_CACHE_MIB": str(CACHE_MIB),
                                       "MALLOC_ARENA_MAX": "2", "RECORD_STORE_LOG": "warn"})
    server.start()
    server.s3().create_bucket(Bucket="memory")
    counters = multiprocessing.Array("q", 2)
    started = time.time()
    stop_at = started + WARM_UP_SECONDS + MEASURE_SECONDS
    workers = [multiprocessing.Process(target=client_process,
                                       args=(server.s3_endpoint, server.credentials.root_access_key,
                                             server.credentials.root_secret_key, stop_at, counters, seed))
               for seed in range(PROCESSES)]
    for worker in workers:
        worker.start()
    samples = []
    while any(worker.is_alive() for worker in workers):
        sample = server.sample()
        sample["t"] = time.time() - started
        sample["requests"] = float(counters[0])
        sample["redb_bytes"] = float(sum(path.stat().st_size for path in server.data_directory.rglob("*.redb")))
        samples.append(sample)
        time.sleep(2)
    for worker in workers:
        worker.join()
    gate.context["samples"] = samples

    # Medians of the first and last 30 s of the measured window, so one
    # allocator hiccup cannot decide the result.
    begin, end = WARM_UP_SECONDS, WARM_UP_SECONDS + MEASURE_SECONDS
    rss_start, rss_end = window(samples, begin, begin + 30, "rss_bytes"), window(samples, end - 30, end, "rss_bytes")
    requests_start = window(samples, begin, begin + 30, "requests")
    requests_end = window(samples, end - 30, end, "requests")
    requests = requests_end - requests_start
    redb_end = samples[-1]["redb_bytes"]
    gate.context.update(rss_start=rss_start, rss_end=rss_end, requests=requests, redb_bytes=redb_end,
                        cache_bytes=CACHE_MIB * 1024 * 1024)
    gate.check("no request failed", counters[1] == 0, {"failed": counters[1]})
    if requests < MINIMUM_REQUESTS:
        raise InvalidMeasurement(f"only {requests:.0f} requests in the measured window; the ratio would be noise")
    gate.check("the databases outgrew the cache several times over", redb_end >= 4 * CACHE_MIB * 1024 * 1024,
               {"redb_bytes": redb_end})
    gate.metric("rss_bytes_per_request_after_warm_up", (rss_end - rss_start) / requests, "B",
                limit=RSS_BYTES_PER_REQUEST_LIMIT,
                basis="an unbounded cache grew ~1300 B/request under this workload (RSG-011); a bounded one does not grow with history")
    gate.metric("peak_rss_bytes", max(sample["rss_bytes"] for sample in samples), "B", limit=256 * 1024 * 1024,
                basis="fits the 512 MiB request the Helm chart makes, with half to spare, at an 8 MiB cache")
    server.stop()


if __name__ == "__main__":
    Gate("RES-MEMORY", "resident memory stops growing with history once the caches are full").run(main)
