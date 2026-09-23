#!/usr/bin/env python3
"""PERF-OVERLOAD: sustained overload is refused cleanly and damages nothing.

The admission contract (record-store.example.toml, [limits]) is: at most
`maximum_concurrent_operations` in flight; an operation that cannot get a slot
within `admission_wait_limit_seconds` is refused with a retryable SlowDown
rather than queued, "which is what otherwise grows without limit under
sustained overload".

The gate shrinks both limits (4 slots, 1 s wait) so that one runner can
saturate them, then holds far more concurrent work than that for a sustained
window -- slow uploads, reads, and slow-header connections that never finish
their request -- while sampling the server's memory, descriptors and
connections. It checks:

  * every refusal is 503 SlowDown and arrives within the wait limit plus a
    margin; nothing is a 500, a reset, or a hang;
  * every write the server acknowledged under load reads back with its bytes,
    and data committed before the load is untouched;
  * once the load stops, descriptors, connections and staging space return to
    their idle level and ordinary requests are served immediately.

The RSS bound is provisional (see gates.toml): there is no documented memory
requirement yet, so it is recorded and compared, not claimed.
"""

from __future__ import annotations

import concurrent.futures
import http.client
import json
import multiprocessing
import random
import urllib.parse
import socket
import statistics
import threading
import time

from botocore.exceptions import BotoCoreError, ClientError

from gatelib import (
    Gate,
    InvalidMeasurement,
    Secrets,
    Server,
    SlowBody,
    artifact_from_arguments,
    identify_artifact,
    read_all,
    sha256_bytes,
)

MIB = 1024 * 1024
SLOTS = 4
WAIT_LIMIT_SECONDS = 1
REJECTION_MARGIN_SECONDS = 1.0
CLIENT_THREADS = 64
SLOW_HEADER_SOCKETS = 48


def probe(url: str, stop_at: float, results) -> None:
    """Runs in its own process: bodiless GETs, timed from outside the load's GIL.

    The load threads hash and trickle multi-megabyte bodies; timing refusals
    inside them measures the client as much as the server. A separate process
    sending requests with no body measures admission.
    """
    parsed = urllib.parse.urlsplit(url)
    samples = []
    while time.time() < stop_at:
        started = time.monotonic()
        try:
            connection = http.client.HTTPConnection(parsed.hostname, parsed.port, timeout=30)
            connection.request("GET", f"{parsed.path}?{parsed.query}")
            response = connection.getresponse()
            body = response.read()
            connection.close()
            kind = "ok" if response.status == 200 else ("slowdown" if response.status == 503 and b"SlowDown" in body else f"http:{response.status}")
        except (OSError, http.client.HTTPException) as error:
            kind = f"transport:{type(error).__name__}"
        samples.append((kind, time.monotonic() - started))
        time.sleep(0.1)
    results.put(samples)


def slow_header_hold_seconds(port: int, cap: float) -> float:
    """How long the server keeps a connection whose headers never finish."""
    connection = socket.create_connection(("127.0.0.1", port), timeout=5)
    connection.sendall(b"GET /overload/committed/00 HTTP/1.1\r\nHost: 127.0.0.1\r\n")
    connection.settimeout(1)
    started = time.monotonic()
    while time.monotonic() - started < cap:
        try:
            connection.sendall(b"X-Slow: 1\r\n")
            if connection.recv(1) == b"":
                break
        except socket.timeout:
            continue
        except OSError:
            break
    else:
        connection.close()
        return cap
    connection.close()
    return time.monotonic() - started


def main(gate: Gate) -> None:
    artifact, options = artifact_from_arguments()
    identify_artifact(gate, artifact)
    duration = 20.0 if options["profile"] == "pr" else 180.0
    rng = random.Random(int(options["seed"]))
    server = Server(
        artifact,
        gate.work_directory / "data",
        gate.evidence_directory / "logs",
        credentials=Secrets(),
        encrypted=True,
        name="overload",
        extra_environment={
            "RECORD_STORE_MAX_CONCURRENT_OPERATIONS": str(SLOTS),
            "RECORD_STORE_ADMISSION_WAIT_LIMIT_SECONDS": str(WAIT_LIMIT_SECONDS),
        },
    )
    server.start()
    client = server.s3(max_pool_connections=CLIENT_THREADS * 2)
    client.create_bucket(Bucket="overload")
    committed = {}
    for index in range(20):
        body = rng.randbytes(256 * 1024)
        client.put_object(Bucket="overload", Key=f"committed/{index:02d}", Body=body)
        committed[f"committed/{index:02d}"] = sha256_bytes(body)
    time.sleep(1)
    idle = server.sample()
    gate.context.update(idle=idle, slots=SLOTS, wait_limit_seconds=WAIT_LIMIT_SECONDS,
                        client_threads=CLIENT_THREADS, slow_header_sockets=SLOW_HEADER_SOCKETS,
                        duration_seconds=duration, encrypted=True)

    outcomes: dict[str, int] = {}
    rejection_latency: list[float] = []
    success_latency: list[float] = []
    acknowledged: dict[str, str] = {}
    unexpected: list[str] = []
    lock = threading.Lock()
    stop = threading.Event()

    def record(kind: str, elapsed: float, detail: str = "") -> None:
        with lock:
            outcomes[kind] = outcomes.get(kind, 0) + 1
            if kind == "slowdown":
                rejection_latency.append(elapsed)
            elif kind == "ok":
                success_latency.append(elapsed)
            elif detail and len(unexpected) < 20:
                unexpected.append(detail)

    def worker(number: int) -> None:
        local = random.Random(number)
        sequence = 0
        while not stop.is_set():
            sequence += 1
            started = time.monotonic()
            try:
                if local.random() < 0.6:
                    key = f"load/{number:02d}/{sequence:05d}"
                    body = local.randbytes(local.choice((64 * 1024, MIB, 4 * MIB)))
                    client.put_object(Bucket="overload", Key=key, Body=SlowBody(body, delay=0.01),
                                      ContentLength=len(body))
                    with lock:
                        acknowledged[key] = sha256_bytes(body)
                else:
                    read_all(client, "overload", f"committed/{local.randrange(20):02d}")
                record("ok", time.monotonic() - started)
            except ClientError as error:
                code = error.response["Error"]["Code"]
                status = error.response["ResponseMetadata"].get("HTTPStatusCode")
                if code == "SlowDown" and status == 503:
                    record("slowdown", time.monotonic() - started)
                else:
                    record(f"error:{code}:{status}", 0, f"{code} {status}")
            except (BotoCoreError, OSError) as error:
                record(f"transport:{type(error).__name__}", 0, str(error)[:200])

    def slow_header(stop_event: threading.Event, sockets: list) -> None:
        for _ in range(SLOW_HEADER_SOCKETS):
            connection = socket.create_connection(("127.0.0.1", server.s3_port), timeout=5)
            connection.sendall(b"GET /overload/committed/00 HTTP/1.1\r\nHost: 127.0.0.1\r\n")
            sockets.append(connection)
        while not stop_event.is_set():
            for connection in list(sockets):
                try:
                    connection.sendall(b"X-Slow: 1\r\n")
                except OSError:
                    sockets.remove(connection)
            stop_event.wait(2)

    samples = []
    held: list[socket.socket] = []
    header_thread = threading.Thread(target=slow_header, args=(stop, held), daemon=True)
    header_thread.start()
    pool = concurrent.futures.ThreadPoolExecutor(max_workers=CLIENT_THREADS)
    futures = [pool.submit(worker, number) for number in range(CLIENT_THREADS)]
    deadline = time.monotonic() + duration
    probe_latency = []
    probe_url = client.generate_presigned_url("get_object", Params={"Bucket": "overload", "Key": "committed/01"}, ExpiresIn=3600)
    probe_results = multiprocessing.Queue()
    time.sleep(2)  # let the load reach saturation before probing it
    prober = multiprocessing.Process(target=probe, args=(probe_url, time.time() + duration - 3, probe_results))
    prober.start()
    while time.monotonic() < deadline:
        samples.append(server.sample())
        time.sleep(0.5)
    probe_samples = probe_results.get(timeout=120)
    prober.join(timeout=30)
    stop.set()
    for future in futures:
        future.result(timeout=120)
    pool.shutdown()
    header_thread.join(timeout=10)
    held_at_end = len(held)
    for connection in held:
        connection.close()

    total = sum(outcomes.values())
    rejected = outcomes.get("slowdown", 0)
    gate.context["outcomes"] = outcomes
    gate.context["unexpected_samples"] = unexpected
    gate.context["slow_header_sockets_still_open_at_end"] = held_at_end
    if rejected == 0 or outcomes.get("ok", 0) == 0:
        raise InvalidMeasurement(f"the load did not produce both admissions and refusals: {outcomes}")

    gate.metric("requests_total", float(total), "requests")
    gate.metric("admitted_ratio", outcomes.get("ok", 0) / total, "ratio")
    gate.check("every refusal under overload is a 503 SlowDown (no 5xx, no reset, no timeout)",
               not unexpected, {"outcomes": outcomes, "samples": unexpected[:5]})
    probe_kinds: dict[str, int] = {}
    for kind, _ in probe_samples:
        probe_kinds[kind] = probe_kinds.get(kind, 0) + 1
    gate.context["probe_outcomes"] = probe_kinds
    refusals = sorted(latency for kind, latency in probe_samples if kind == "slowdown")
    admitted = sorted(latency for kind, latency in probe_samples if kind == "ok")
    gate.check("every probe during overload was admitted or refused with SlowDown",
               set(probe_kinds) <= {"ok", "slowdown"}, probe_kinds)
    if len(refusals) < 5:
        raise InvalidMeasurement(f"the probe saw too few refusals to measure them: {probe_kinds}")
    gate.metric("probe_slowdown_latency_max_seconds", refusals[-1], "s",
                limit=WAIT_LIMIT_SECONDS + REJECTION_MARGIN_SECONDS,
                basis="a bodiless request is refused within admission_wait_limit_seconds plus a 1 s margin")
    gate.metric("probe_admitted_latency_p50_seconds", admitted[len(admitted) // 2] if admitted else float("nan"), "s")
    gate.metric("probe_admitted_latency_max_seconds", admitted[-1] if admitted else float("nan"), "s",
                limit=WAIT_LIMIT_SECONDS + REJECTION_MARGIN_SECONDS,
                basis="an admitted request waited no longer than the admission limit allows")
    if rejection_latency:
        rejection_latency.sort()
        gate.metric("load_thread_slowdown_latency_p99_seconds",
                    rejection_latency[int(0.99 * (len(rejection_latency) - 1))], "s")
    gate.metric("admitted_latency_median_seconds", statistics.median(success_latency), "s")

    peak_rss = max(sample["rss_bytes"] for sample in samples)
    peak_files = max(sample["open_files"] for sample in samples)
    gate.metric("peak_rss_growth_bytes", peak_rss - idle["rss_bytes"], "bytes", limit=512 * MIB,
                provisional=True, enforcement="advisory",
                basis="provisional: no documented memory requirement; calibrate from scheduled runs")
    gate.metric("peak_open_files", peak_files, "files")

    # Recovery after the load. The load's own keep-alive pool is closed first:
    # those connections are the client's, and counting them would report the
    # gate's pool as a server leak.
    client.close()
    client = server.s3()
    recovered_at = None
    started = time.monotonic()
    while time.monotonic() - started < 15:
        sample = server.sample()
        if sample["open_files"] <= idle["open_files"] + 8 and sample["connections"] <= idle["connections"] + 4:
            recovered_at = time.monotonic() - started
            break
        time.sleep(0.5)
    after = server.sample()
    gate.context["after"] = after
    gate.check("descriptors and connections return to idle within 15 s of the load stopping",
               recovered_at is not None, {"idle": idle, "after": after})
    probe_started = time.monotonic()
    ok = read_all(client, "overload", "committed/00")
    probe_latency.append(time.monotonic() - probe_started)
    gate.metric("first_request_after_overload_seconds", probe_latency[-1], "s", limit=1.0,
                basis="once load stops nothing should still be queued ahead of a new request")
    gate.check("an ordinary read succeeds after the load", sha256_bytes(ok) == committed["committed/00"])

    wrong = [k for k, d in committed.items() if sha256_bytes(read_all(client, "overload", k)) != d]
    gate.check("data committed before the load is untouched", not wrong, wrong[:5])
    damaged = []
    for key, digest in acknowledged.items():
        try:
            if sha256_bytes(read_all(client, "overload", key)) != digest:
                damaged.append(key)
        except Exception as error:  # noqa: BLE001
            damaged.append(f"{key}: {type(error).__name__}")
    gate.metric("writes_acknowledged_under_load", float(len(acknowledged)), "objects")
    gate.check("every write acknowledged under load reads back with its bytes", not damaged, damaged[:5])
    hold = slow_header_hold_seconds(server.s3_port, cap=75.0)
    # Advisory, not blocking: docs/deployment/reverse-proxy.md requires a TLS
    # terminator in front of the public endpoints, and that proxy enforces its
    # own client header timeout. Direct exposure is not a supported topology.
    # The finding is tracked as release/findings/RSG-004.
    gate.metric("slow_header_connection_hold_seconds", hold, "s", limit=60.0, provisional=True,
                enforcement="advisory",
                basis="a client that never completes its headers should be disconnected (RSG-004)")
    status = server.api("GET", "/api/v1/storage/status")
    gate.check("no staging space is held once the load has stopped", status.get("temporary_upload_bytes") == 0, status)
    server.stop()


if __name__ == "__main__":
    Gate("PERF-OVERLOAD", "overload is refused with SlowDown, bounded, and recovers").run(main)
