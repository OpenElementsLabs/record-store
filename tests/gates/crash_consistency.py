#!/usr/bin/env python3
"""REC-CRASH: a killed process loses nothing it acknowledged and publishes nothing partial.

The public durability contract says a successful write survives a process crash
and that a partially written payload is never visible as an object. This gate
checks both against the real binary: it keeps uploads, overwrites, new versions,
multipart parts and a stream of small writes in flight, SIGKILLs the server at a
seeded random moment, restarts it on the same data directory, and compares what
it serves with what was acknowledged before the kill.

It verifies final bytes, not status codes. Every object that answers is read in
full and compared by SHA-256; a listing is walked to the end and must agree
exactly with what reads return.

What it does not show: SIGKILL stops the process, not the machine. The kernel
still flushes the page cache, so this is evidence for crash safety and says
nothing about power loss, which needs an fsync-honouring device under real
power interruption. Do not cite it for that.
"""

from __future__ import annotations

import concurrent.futures
import os
import random
import threading
import time
import urllib.parse
import uuid
from dataclasses import dataclass, field

from botocore.exceptions import BotoCoreError, ClientError

from gatelib import (
    Gate,
    GateFailure,
    ReadFailed,
    InvalidMeasurement,
    Secrets,
    Server,
    SlowBody,
    all_events,
    artifact_from_arguments,
    audit_chain_intact,
    identify_artifact,
    list_all_keys,
    read_all,
    sha256_bytes,
)

MIB = 1024 * 1024
# Container health contract: deploy/docker/Dockerfile gives the process a 5 s
# start period before health checks count. A crash restart slower than that on
# this small dataset would show as unhealthy to every orchestrator using it.
RESTART_READY_LIMIT_SECONDS = 5.0


@dataclass
class Expected:
    """What the server has acknowledged, and what it may or may not have kept."""

    objects: dict[tuple[str, str], str] = field(default_factory=dict)  # (bucket,key) -> sha
    versions: dict[tuple[str, str, str], str] = field(default_factory=dict)  # (bucket,key,version) -> sha
    ambiguous: dict[tuple[str, str], set[str]] = field(default_factory=dict)  # may hold any of these
    acknowledged_writes: list[tuple[str, str]] = field(default_factory=list)
    lock: threading.Lock = field(default_factory=threading.Lock)

    def acknowledge(self, bucket: str, key: str, digest: str, version: str | None = None) -> None:
        with self.lock:
            self.objects[(bucket, key)] = digest
            self.ambiguous.pop((bucket, key), None)
            if version:
                self.versions[(bucket, key, version)] = digest
            self.acknowledged_writes.append((bucket, key))

    def maybe(self, bucket: str, key: str, digest: str) -> None:
        with self.lock:
            allowed = self.ambiguous.setdefault((bucket, key), set())
            if (bucket, key) in self.objects:
                allowed.add(self.objects[(bucket, key)])
            allowed.add(digest)


def seed_committed(gate: Gate, server: Server, rng: random.Random, expected: Expected) -> None:
    client = server.s3()
    client.create_bucket(Bucket="committed")
    client.create_bucket(Bucket="versioned")
    client.put_bucket_versioning(Bucket="versioned", VersioningConfiguration={"Status": "Enabled"})
    for index in range(40):
        body = rng.randbytes(rng.randint(0, 64 * 1024))
        client.put_object(Bucket="committed", Key=f"small/{index:03d}", Body=body)
        expected.acknowledge("committed", f"small/{index:03d}", sha256_bytes(body))
    for index in range(2):
        body = rng.randbytes(6 * MIB)
        client.put_object(Bucket="committed", Key=f"large/{index}", Body=body)
        expected.acknowledge("committed", f"large/{index}", sha256_bytes(body))
    target = rng.randbytes(2 * MIB)
    client.put_object(Bucket="committed", Key="overwrite-target", Body=target)
    expected.acknowledge("committed", "overwrite-target", sha256_bytes(target))
    for _ in range(3):
        body = rng.randbytes(rng.randint(1, 256 * 1024))
        version = client.put_object(Bucket="versioned", Key="doc", Body=body)["VersionId"]
        expected.acknowledge("versioned", "doc", sha256_bytes(body), version)
    upload = client.create_multipart_upload(Bucket="committed", Key="multipart-done")
    first, second = rng.randbytes(5 * MIB), rng.randbytes(MIB)
    parts = []
    for number, body in ((1, first), (2, second)):
        etag = client.upload_part(
            Bucket="committed", Key="multipart-done", UploadId=upload["UploadId"], PartNumber=number, Body=body
        )["ETag"]
        parts.append({"PartNumber": number, "ETag": etag})
    client.complete_multipart_upload(
        Bucket="committed", Key="multipart-done", UploadId=upload["UploadId"], MultipartUpload={"Parts": parts}
    )
    expected.acknowledge("committed", "multipart-done", sha256_bytes(first + second))
    gate.context.setdefault("seeded_objects", len(expected.objects))


def crash_iteration(gate: Gate, server: Server, rng: random.Random, expected: Expected, iteration: int, multipart: dict) -> dict:
    """Starts writes, kills the server under them, and records what was acknowledged."""
    client = server.s3()
    stop = threading.Event()
    in_flight_bytes = 0
    jobs = []

    def slow_put(bucket: str, key: str, body: bytes, version_key: bool = False):
        digest = sha256_bytes(body)
        expected.maybe(bucket, key, digest)
        try:
            response = client.put_object(Bucket=bucket, Key=key, Body=SlowBody(body), ContentLength=len(body))
        except (BotoCoreError, ClientError, OSError):
            return
        expected.acknowledge(bucket, key, digest, response.get("VersionId") if version_key else None)

    def burst():
        index = 0
        while not stop.is_set():
            key = f"burst/{iteration}/{index:05d}"
            body = rng.randbytes(rng.randint(0, 8 * 1024))
            digest = sha256_bytes(body)
            expected.maybe("committed", key, digest)
            try:
                client.put_object(Bucket="committed", Key=key, Body=body)
            except (BotoCoreError, ClientError, OSError):
                return
            expected.acknowledge("committed", key, digest)
            index += 1

    # One multipart upload is carried across the crash: a part acknowledged
    # before the kill must still be listed afterwards.
    upload_id = client.create_multipart_upload(Bucket="committed", Key=f"multipart-open/{iteration}")["UploadId"]
    part_one = rng.randbytes(5 * MIB)
    etag = client.upload_part(
        Bucket="committed", Key=f"multipart-open/{iteration}", UploadId=upload_id, PartNumber=1, Body=part_one
    )["ETag"]
    multipart[upload_id] = {"key": f"multipart-open/{iteration}", "parts": {1: (etag, part_one)}}

    def slow_part():
        body = rng.randbytes(5 * MIB)
        try:
            etag = client.upload_part(
                Bucket="committed",
                Key=f"multipart-open/{iteration}",
                UploadId=upload_id,
                PartNumber=2,
                Body=SlowBody(body),
                ContentLength=len(body),
            )["ETag"]
        except (BotoCoreError, ClientError, OSError):
            multipart[upload_id]["maybe_part"] = (2, body)
            return
        multipart[upload_id]["parts"][2] = (etag, body)

    pool = concurrent.futures.ThreadPoolExecutor(max_workers=8)
    for index in range(3):
        body = rng.randbytes(24 * MIB)
        in_flight_bytes += len(body)
        jobs.append(pool.submit(slow_put, "committed", f"inflight/{iteration}/{index}", body))
    overwrite = rng.randbytes(8 * MIB)
    in_flight_bytes += len(overwrite)
    jobs.append(pool.submit(slow_put, "committed", "overwrite-target", overwrite))
    version_body = rng.randbytes(8 * MIB)
    in_flight_bytes += len(version_body)
    jobs.append(pool.submit(slow_put, "versioned", "doc", version_body, True))
    jobs.append(pool.submit(slow_part))
    jobs.append(pool.submit(burst))

    delay = rng.uniform(0.3, 1.8)
    time.sleep(delay)
    server.kill()
    stop.set()
    # Measured on disk with the process dead and before anything can tidy up:
    # the proof that the kill landed while payload bytes were being staged.
    staged_files, staged_bytes = staging_state(server)
    inject_defect(server)
    for job in jobs:
        job.result(timeout=120)
    pool.shutdown()
    return {
        "kill_after_seconds": round(delay, 3),
        "in_flight_bytes": in_flight_bytes,
        "staged_files_at_kill": staged_files,
        "staged_bytes_at_kill": staged_bytes,
    }


def inject_defect(server: Server) -> None:
    """The gate's negative control. `GATE_INJECT_DEFECT` damages the data
    directory while the process is dead, the way a lost write would, so that a
    self-test can prove this gate fails when a committed payload is lost or
    altered. Never set outside `tests/gates/selftest.sh`.
    """
    defect = os.environ.get("GATE_INJECT_DEFECT")
    if not defect:
        return
    payloads = sorted(path for path in (server.data_directory / "objects").rglob("*") if path.is_file())
    if not payloads:
        return
    victim = payloads[len(payloads) // 2]
    if defect == "delete-payload":
        victim.unlink()
    elif defect == "flip-byte":
        data = bytearray(victim.read_bytes())
        if data:
            data[len(data) // 2] ^= 0xFF
            victim.write_bytes(bytes(data))


def staging_state(server: Server) -> tuple[int, int]:
    staging = server.data_directory / "tmp"
    files = [path for path in staging.rglob("*") if path.is_file()] if staging.exists() else []
    return len(files), sum(path.stat().st_size for path in files)


def verify(gate: Gate, server: Server, expected: Expected, multipart: dict, label: str) -> None:
    client = server.s3()
    mismatched, missing = [], []
    for (bucket, key), digest in sorted(expected.objects.items()):
        try:
            actual = sha256_bytes(read_all(client, bucket, key))
        except ClientError as error:
            missing.append(f"{bucket}/{key}: {error.response['Error']['Code']}")
            continue
        except ReadFailed as error:
            missing.append(str(error))
            continue
        if actual != digest and actual not in expected.ambiguous.get((bucket, key), set()):
            mismatched.append(f"{bucket}/{key}")
    gate.check(f"{label}: every acknowledged object is readable", not missing, missing[:10])
    gate.check(f"{label}: every acknowledged object has its acknowledged bytes", not mismatched, mismatched[:10])

    partial = []
    for (bucket, key), allowed in sorted(expected.ambiguous.items()):
        try:
            actual = sha256_bytes(read_all(client, bucket, key))
        except ClientError as error:
            if error.response["Error"]["Code"] in ("NoSuchKey", "404"):
                continue
            partial.append(f"{bucket}/{key}: {error.response['Error']['Code']}")
            continue
        except ReadFailed as error:
            partial.append(str(error))
            continue
        if actual not in allowed:
            partial.append(f"{bucket}/{key}")
    gate.check(f"{label}: an unacknowledged write is absent or complete, never partial", not partial, partial[:10])

    large = read_all(client, "committed", "large/0")
    ranged = read_all(client, "committed", "large/0", Range="bytes=1048570-2097160")
    gate.check(f"{label}: a ranged read returns the stored slice", ranged == large[1048570:2097161])

    lost_versions = []
    for (bucket, key, version), digest in sorted(expected.versions.items()):
        try:
            actual = sha256_bytes(read_all(client, bucket, key, VersionId=version))
        except (ClientError, ReadFailed):
            lost_versions.append(version)
            continue
        if actual != digest:
            lost_versions.append(version)
    gate.check(f"{label}: every acknowledged version reads back with its bytes", not lost_versions, lost_versions)

    for bucket in ("committed", "versioned"):
        listed = set(list_all_keys(client, bucket))
        readable = {key for (b, key) in expected.objects if b == bucket}
        for (b, key) in expected.ambiguous:
            if b == bucket:
                try:
                    client.head_object(Bucket=bucket, Key=key)
                    readable.add(key)
                except ClientError:
                    pass
        gate.check(
            f"{label}: listing {bucket} to the end agrees exactly with what reads return",
            listed == readable,
            {"listed_only": sorted(listed - readable)[:10], "readable_only": sorted(readable - listed)[:10]},
        )

    lost_parts = []
    for upload_id, state in multipart.items():
        try:
            listed = client.list_parts(Bucket="committed", Key=state["key"], UploadId=upload_id)
        except ClientError as error:
            lost_parts.append(f"{upload_id}: {error.response['Error']['Code']}")
            continue
        numbers = {part["PartNumber"]: part["ETag"] for part in listed.get("Parts", [])}
        for number, (etag, _) in state["parts"].items():
            if numbers.get(number) != etag:
                lost_parts.append(f"{upload_id} part {number}")
    gate.check(f"{label}: every acknowledged multipart part is still listed", not lost_parts, lost_parts)

    inspection = server.api("GET", "/api/v1/storage/inspect?maximum_entries=1000000")
    gate.check(
        f"{label}: no catalog entry references a missing payload",
        inspection.get("metadata_without_data") == 0,
        inspection,
    )
    gate.check(f"{label}: storage inspection was not truncated", inspection.get("truncated") is False)

    # A committed change always produces its event (administration/events-and-webhooks.md).
    deadline = time.monotonic() + 20
    missing_events: list[str] = []
    while True:
        seen: dict[tuple[str, str], set[str]] = {}
        duplicates = []
        by_id: dict[str, tuple] = {}
        for bucket in ("committed", "versioned"):
            for event in all_events(server, bucket):
                if event.get("object"):
                    seen.setdefault((bucket, event["object"]), set()).add(event["type"])
                identity = (event["type"], event.get("bucket"), event.get("object"), event.get("version_id"))
                if event["id"] in by_id and by_id[event["id"]] != identity:
                    duplicates.append(event["id"])
                by_id[event["id"]] = identity
        missing_events = sorted(
            f"{b}/{k}"
            for (b, k) in set(expected.acknowledged_writes)
            if not ({"object.created", "object.updated", "multipart.completed"} & seen.get((b, k), set()))
        )
        if not missing_events or time.monotonic() > deadline:
            break
        time.sleep(1)
    # The paginated walk can lose an event at every page boundary.
    # Each event it did not return is looked up again with its exact key as the
    # prefix -- a single page, no boundary -- so a pagination defect and a
    # crash that really lost an event are reported as different failures.
    lost = []
    for entry in missing_events:
        bucket, key = entry.split("/", 1)
        page = server.api("GET", "/api/v1/events?" + urllib.parse.urlencode({"bucket": bucket, "prefix": key, "limit": "1000"}))
        kinds = {e["type"] for e in page.get("events", []) if e.get("object") == key}
        if not ({"object.created", "object.updated", "multipart.completed"} & kinds):
            lost.append(entry)
    gate.check(f"{label}: every acknowledged write produced its storage event", not lost, lost[:10])
    gate.check(f"{label}: the paginated storage-event walk returns every event",
               not missing_events, {"not returned by the walk but present": sorted(set(missing_events) - set(lost))[:10]})
    gate.check(f"{label}: an event id never names two different changes", not duplicates, duplicates[:5])

    intact, chain = audit_chain_intact(server)
    gate.check(f"{label}: the audit hash chain is intact", intact, chain)


def main(gate: Gate) -> None:
    artifact, options = artifact_from_arguments()
    identify_artifact(gate, artifact)
    seed = int(options["seed"])
    iterations = 2 if options["profile"] == "pr" else 5
    gate.context.update(seed=seed, iterations_per_mode=iterations, profile=options["profile"])
    restart_times: list[float] = []

    for encrypted in (False, True):
        mode = "encrypted" if encrypted else "plaintext"
        rng = random.Random(f"{seed}-{mode}")
        data = gate.work_directory / mode / "data"
        server = Server(
            artifact,
            data,
            gate.evidence_directory / "logs",
            credentials=Secrets(),
            encrypted=encrypted,
            name=f"crash-{mode}",
        )
        server.start()
        expected = Expected()
        multipart: dict = {}
        seed_committed(gate, server, rng, expected)
        crashes = []
        for iteration in range(iterations):
            crashes.append(crash_iteration(gate, server, rng, expected, iteration, multipart))
            restart_times.append(server.start())
            crashes[-1]["staged_bytes_after_restart"] = staging_state(server)[1]
            verify(gate, server, expected, multipart, f"{mode} crash {iteration + 1}")
        gate.context[f"{mode}_crashes"] = crashes
        # A kill that never lands while payload bytes are being staged tests
        # the easy case only. At least half the kills must catch at least 1 MiB
        # of an interrupted upload on disk, or the run is not evidence.
        landed = sum(1 for crash in crashes if crash["staged_bytes_at_kill"] >= MIB)
        gate.context[f"{mode}_kills_mid_staging"] = landed
        if landed * 2 < len(crashes):
            raise InvalidMeasurement(f"{mode}: only {landed} of {len(crashes)} kills landed mid-upload")
        gate.check(
            f"{mode}: staging left by killed uploads is gone after restart",
            all(crash["staged_bytes_after_restart"] == 0 for crash in crashes),
            [crash["staged_bytes_after_restart"] for crash in crashes],
        )
        gate.context[f"{mode}_acknowledged_objects"] = len(expected.objects)

        # Residue left by writes the kill interrupted. A single PUT that dies
        # mid-body leaves its staging file behind (SIGKILL runs no destructor),
        # so it has to be reclaimable by something, or repeated crashes grow the
        # disk without bound.
        client = server.s3()
        for upload_id, state in multipart.items():
            client.abort_multipart_upload(Bucket="committed", Key=state["key"], UploadId=upload_id)
        before = server.api("GET", "/api/v1/storage/status")
        inspection_before = server.api("GET", "/api/v1/storage/inspect?maximum_entries=1000000")
        repair = server.api("POST", "/api/v1/storage/repair", {"maximum_entries": 1000000, "dry_run": False})
        after = server.api("GET", "/api/v1/storage/status")
        inspection_after = server.api("GET", "/api/v1/storage/inspect?maximum_entries=1000000")
        gate.context[f"{mode}_residue"] = {
            "before_repair": {"status": before, "inspection": inspection_before},
            "repair": repair,
            "after_repair": {"status": after, "inspection": inspection_after},
        }
        gate.metric(
            f"{mode}_temporary_bytes_after_crashes_before_repair",
            float(before.get("temporary_upload_bytes", 0)),
            "bytes",
        )
        gate.metric(
            f"{mode}_temporary_bytes_after_abort_and_repair",
            float(after.get("temporary_upload_bytes", 0)),
            "bytes",
            limit=0,
            basis="with no upload in progress, interrupted writes must leave nothing an operator cannot reclaim",
        )
        gate.check(
            f"{mode}: no orphaned payloads remain after repair",
            inspection_after.get("data_without_metadata") == 0 and inspection_after.get("unknown_data_entries") == 0,
            inspection_after,
        )
        for bucket in ("committed", "versioned"):
            result = server.api("POST", f"/api/v1/verify/buckets/{bucket}")
            gate.check(f"{mode}: full integrity verification of {bucket} finds no failure", result.get("failures") == 0, result)
        server.stop()

    # Deterministic companion to the random kills: the exact state a SIGKILL
    # leaves between creating a publication record and writing it (an empty
    # tmp/<id>.publish). Random kills reach that window only occasionally, so
    # it is placed here on every run.
    for encrypted in (False, True):
        mode = "encrypted" if encrypted else "plaintext"
        server = Server(artifact, gate.work_directory / f"torn-{mode}" / "data", gate.evidence_directory / "logs",
                        credentials=Secrets(), encrypted=encrypted, name=f"torn-record-{mode}")
        server.start()
        client = server.s3()
        client.create_bucket(Bucket="torn")
        client.put_object(Bucket="torn", Key="committed", Body=b"committed before the kill")
        server.stop()
        (server.data_directory / "tmp" / f"{uuid.uuid4().hex}.publish").write_bytes(b"")
        try:
            server.start()
            started = True
        except GateFailure:
            started = False
        gate.check(f"{mode}: a publication record torn by a kill does not prevent start-up", started,
                   "see the torn-record log: start-up refused an empty .publish record")
        if started:
            gate.check(f"{mode}: committed data is served after recovering from a torn record",
                       read_all(server.s3(), "torn", "committed") == b"committed before the kill")
            server.stop()

    gate.metric(
        "crash_restart_to_ready_seconds_max",
        max(restart_times),
        "s",
        limit=RESTART_READY_LIMIT_SECONDS,
        basis="container start period in deploy/docker/Dockerfile HEALTHCHECK is 5 s",
    )


if __name__ == "__main__":
    Gate("REC-CRASH", "acknowledged writes survive SIGKILL; nothing partial is published").run(main)
