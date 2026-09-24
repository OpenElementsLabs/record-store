#!/usr/bin/env python3
"""COR-INTEGRITY: a damaged payload is never served as if it were intact.

docs/concepts/durability.md states the read-side contract precisely:

  * whole-object read: length checked before any byte is sent, then SHA-256
    recomputed while streaming; "a mismatch fails the read before its last
    chunk";
  * encrypted ranged read: each chunk's authentication tag checked;
  * unencrypted ranged read: length only -- a same-length edit inside the range
    is *not* detected by the read (a stated limitation, recorded, not asserted).

This gate damages stored payloads in an isolated deployment the way bit rot or
a bad disk would -- one flipped byte, a truncation, a deletion -- and checks
what a client actually receives over plain HTTP. "Fails" means the client can
tell: a non-2xx status, or a body shorter than Content-Length. A 200 with a
complete body of the wrong bytes is the failure this gate exists to catch.
"""

from __future__ import annotations

import http.client
import random
import urllib.parse

from gatelib import (
    Gate,
    Secrets,
    Server,
    artifact_from_arguments,
    identify_artifact,
    read_all,
    sha256_bytes,
)

MIB = 1024 * 1024
SIZES = {"single-chunk": 1000, "multi-chunk": 3 * MIB + 17, "large": 20 * MIB + 3}


def raw_get(server: Server, client, key: str, byte_range: str | None = None) -> dict:
    """GETs over a bare HTTP connection and reports exactly what arrived."""
    url = client.generate_presigned_url("get_object", Params={"Bucket": "integrity", "Key": key}, ExpiresIn=300)
    parsed = urllib.parse.urlsplit(url)
    connection = http.client.HTTPConnection(parsed.hostname, parsed.port, timeout=60)
    headers = {"Range": byte_range} if byte_range else {}
    connection.request("GET", f"{parsed.path}?{parsed.query}", headers=headers)
    response = connection.getresponse()
    declared = response.getheader("content-length")
    try:
        body = response.read()
        broken = False
    except http.client.IncompleteRead as error:
        body = error.partial
        broken = True
    except (ConnectionError, OSError):
        body = b""
        broken = True
    connection.close()
    return {"status": response.status, "declared": int(declared) if declared else None, "body": body, "broken": broken}


def client_can_tell(result: dict) -> bool:
    """True when a client following HTTP semantics would see the read fail."""
    if result["status"] >= 300:
        return True
    if result["broken"]:
        return True
    return result["declared"] is not None and len(result["body"]) < result["declared"]


def damage(path, how: str) -> None:
    data = bytearray(path.read_bytes())
    if how == "flip":
        data[len(data) // 2] ^= 0x01
        path.write_bytes(bytes(data))
    elif how == "truncate":
        path.write_bytes(bytes(data[: len(data) - 1]))
    elif how == "delete":
        path.unlink()


def main(gate: Gate) -> None:
    artifact, options = artifact_from_arguments()
    identify_artifact(gate, artifact)
    rng = random.Random(int(options["seed"]))
    outcomes: dict[str, dict] = {}

    for encrypted in (False, True):
        mode = "encrypted" if encrypted else "plaintext"
        server = Server(
            artifact,
            gate.work_directory / mode / "data",
            gate.evidence_directory / "logs",
            credentials=Secrets(),
            encrypted=encrypted,
            name=f"integrity-{mode}",
        )
        server.start()
        client = server.s3()
        client.create_bucket(Bucket="integrity")
        originals: dict[str, bytes] = {}
        plan = [(f"{how}/{label}", how, size) for how in ("flip", "truncate", "delete") for label, size in SIZES.items()]
        plan.append(("control/untouched", None, 2 * MIB))
        payload_of: dict[str, object] = {}
        for key, _, size in plan:
            before = {p for p in (server.data_directory / "objects").rglob("*") if p.is_file()}
            originals[key] = rng.randbytes(size)
            client.put_object(Bucket="integrity", Key=key, Body=originals[key])
            after = {p for p in (server.data_directory / "objects").rglob("*") if p.is_file()}
            (payload_of[key],) = after - before
        server.stop()
        for key, how, _ in plan:
            if how:
                damage(payload_of[key], how)
        server.start()

        for key, how, size in plan:
            whole = raw_get(server, client, key)
            record = {
                "status": whole["status"],
                "declared": whole["declared"],
                "received": len(whole["body"]),
                "broken": whole["broken"],
                "matches_original": whole["body"] == originals[key],
            }
            outcomes[f"{mode}:{key}"] = record
            if how is None:
                gate.check(f"{mode}: an undamaged object reads back intact", whole["status"] == 200 and record["matches_original"])
                continue
            damage_name = {"flip": "one flipped byte", "truncate": "a truncation", "delete": "a deleted file"}[how]
            gate.check(
                f"{mode}: whole read of a payload with {damage_name} fails visibly ({key.split('/')[1]})",
                client_can_tell(whole),
                {k: v for k, v in record.items()},
            )
            if how == "truncate":
                gate.check(
                    f"{mode}: a truncated payload sends no body ({key.split('/')[1]})",
                    whole["status"] >= 300 or len(whole["body"]) == 0,
                    record,
                )

        # Ranged reads of the flipped multi-chunk object.
        key = "flip/multi-chunk"
        middle = (SIZES["multi-chunk"]) // 2
        inside = raw_get(server, client, key, f"bytes={middle - 100}-{middle + 100}")
        far = raw_get(server, client, key, "bytes=0-999")
        if encrypted:
            gate.check(
                "encrypted: a ranged read over the damaged chunk fails visibly",
                client_can_tell(inside),
                {"status": inside["status"], "received": len(inside["body"])},
            )
        else:
            detected = client_can_tell(inside)
            gate.note(
                "plaintext ranged read over a same-length edit "
                + ("was detected" if detected else "was served without detection -- documented limitation")
            )
        gate.check(
            f"{mode}: a ranged read away from the damage returns the stored bytes",
            far["status"] == 206 and far["body"] == originals[key][0:1000],
            {"status": far["status"]},
        )
        # The SDK path agrees with the raw one for the undamaged object.
        gate.check(
            f"{mode}: the SDK reads the undamaged object intact",
            sha256_bytes(read_all(client, "integrity", "control/untouched")) == sha256_bytes(originals["control/untouched"]),
        )
        verification = server.api("POST", "/api/v1/verify/buckets/integrity")
        damaged = sum(1 for _, how, _ in plan if how)
        gate.check(
            f"{mode}: bucket verification counts every damaged payload",
            verification.get("failures") == damaged,
            {"verification": verification, "damaged": damaged},
        )
        server.stop()
    gate.context["outcomes"] = outcomes


if __name__ == "__main__":
    Gate("COR-INTEGRITY", "a damaged payload is never served as if it were intact").run(main)
