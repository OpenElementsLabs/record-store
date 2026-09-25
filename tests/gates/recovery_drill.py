#!/usr/bin/env python3
"""REC-BACKUP: a backup taken with the shipped CLI restores into an equivalent deployment.

docs/operations/backup-and-restore.md is the recovery contract: an offline,
exclusive-lock backup; three verification levels; a restore that verifies before
writing, refuses a populated directory, refuses the wrong master key, and blocks
start-up if interrupted until it is re-run. This gate follows that document
step by step with the real `record-store` binary and its exit codes, on an
isolated deployment built for the run, and then checks the restored deployment
the way its users would: object bytes, version history, delete markers,
retention and legal holds, service-account credentials, share links, the audit
chain, and the storage-event journal.

Recovery point: the backup is offline, so the recovery point is the moment the
server was stopped. The gate asserts that exactly: everything acknowledged
before the stop is restored, and a write made after the backup is absent.
Recovery time is measured from the start of `restore` to the first verified read
on the restored deployment, on the dataset described in the evidence.
"""

from __future__ import annotations

import datetime
import json
import os
import random
import shutil
import signal
import subprocess
import time
import urllib.request

from botocore.exceptions import ClientError

from gatelib import (
    Gate,
    InvalidMeasurement,
    Secrets,
    Server,
    all_events,
    artifact_from_arguments,
    audit_chain_intact,
    filesystem_of,
    identify_artifact,
    list_all_keys,
    read_all,
    sha256_bytes,
)

MIB = 1024 * 1024
EXIT_CONFIGURATION = 2
EXIT_UNUSABLE = 3
EXIT_CONFLICT = 4
EXIT_IN_USE = 6


def populate(gate: Gate, server: Server, rng: random.Random, large_objects: int) -> dict:
    """Builds every kind of state the recovery contract says a restore carries."""
    # Random payloads have no inline-safe content type, so share links are read
    # with ?download=true (docs/security/sharing-security.md).
    client = server.s3()
    state: dict = {"objects": {}, "versions": {}, "markers": [], "locks": {}}

    client.create_bucket(Bucket="drill-data")
    for index in range(60):
        body = rng.randbytes(rng.randint(0, 128 * 1024))
        client.put_object(Bucket="drill-data", Key=f"small/{index:03d}", Body=body)
        state["objects"][("drill-data", f"small/{index:03d}")] = sha256_bytes(body)
    for index in range(large_objects):
        body = rng.randbytes(16 * MIB)
        client.put_object(Bucket="drill-data", Key=f"large/{index}", Body=body)
        state["objects"][("drill-data", f"large/{index}")] = sha256_bytes(body)

    client.create_bucket(Bucket="drill-versions")
    client.put_bucket_versioning(Bucket="drill-versions", VersioningConfiguration={"Status": "Enabled"})
    for key in ("contract", "ledger"):
        for _ in range(3):
            body = rng.randbytes(rng.randint(1, 64 * 1024))
            version = client.put_object(Bucket="drill-versions", Key=key, Body=body)["VersionId"]
            state["versions"][("drill-versions", key, version)] = sha256_bytes(body)
    marker = client.delete_object(Bucket="drill-versions", Key="ledger")
    state["markers"].append(("drill-versions", "ledger", marker["VersionId"]))
    state["objects"][("drill-versions", "contract")] = [
        digest for (b, k, _), digest in state["versions"].items() if k == "contract"
    ][-1]

    client.create_bucket(Bucket="drill-locked", ObjectLockEnabledForBucket=True)
    retain_until = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(days=30)
    body = rng.randbytes(4096)
    version = client.put_object(
        Bucket="drill-locked",
        Key="retained",
        Body=body,
        ObjectLockMode="COMPLIANCE",
        ObjectLockRetainUntilDate=retain_until,
    )["VersionId"]
    state["locks"]["retained"] = {"version": version, "mode": "COMPLIANCE", "until": retain_until.replace(microsecond=0)}
    state["objects"][("drill-locked", "retained")] = sha256_bytes(body)
    body = rng.randbytes(4096)
    version = client.put_object(Bucket="drill-locked", Key="held", Body=body, ObjectLockLegalHoldStatus="ON")["VersionId"]
    state["locks"]["held"] = {"version": version, "hold": "ON"}
    state["objects"][("drill-locked", "held")] = sha256_bytes(body)

    account = server.api("POST", "/api/v1/service-accounts", {"name": "restore-drill"})
    state["credential"] = (account["credential"]["key_id"], account["secret_access_key"])

    share = server.api("POST", "/api/v1/buckets/drill-data/object-shares/small/001", {"label": "restore-drill"})
    state["share_token"] = share["url"].rsplit("/", 1)[-1]
    state["share_digest"] = state["objects"][("drill-data", "small/001")]
    status, body = share_content(server, state["share_token"])
    gate.require("the share link serves the object before any backup", status == 200 and sha256_bytes(body) == state["share_digest"], f"HTTP {status}")
    gate.context["dataset"] = {
        "objects": len(state["objects"]),
        "versions": len(state["versions"]),
        "large_objects_16MiB": large_objects,
    }
    return state


def share_content(server: Server, token: str) -> tuple[int, bytes]:
    try:
        with urllib.request.urlopen(f"{server.api_endpoint}/s/{token}/content?download=true", timeout=30) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as error:
        return error.code, b""


def credential_authenticates(server: Server, key_id: str, secret: str) -> str:
    """Distinguishes 'known and correctly sealed' from 'unknown' or 'wrong secret'.

    The drill account holds no policy, so an authenticated request is refused
    with AccessDenied. InvalidAccessKeyId or SignatureDoesNotMatch would mean
    the credential did not survive, or was unsealed with the wrong key.
    """
    try:
        server.s3(key_id, secret).list_objects_v2(Bucket="drill-data")
        return "allowed"
    except ClientError as error:
        return error.response["Error"]["Code"]


def verify_restored(gate: Gate, server: Server, state: dict, label: str) -> None:
    client = server.s3()
    wrong = []
    for (bucket, key), digest in sorted(state["objects"].items()):
        try:
            if sha256_bytes(read_all(client, bucket, key)) != digest:
                wrong.append(f"{bucket}/{key}")
        except Exception as error:  # noqa: BLE001
            wrong.append(f"{bucket}/{key}: {type(error).__name__}")
    gate.check(f"{label}: every object is restored with its bytes", not wrong, wrong[:10])

    lost = []
    for (bucket, key, version), digest in sorted(state["versions"].items()):
        try:
            if sha256_bytes(read_all(client, bucket, key, VersionId=version)) != digest:
                lost.append(version)
        except Exception:  # noqa: BLE001
            lost.append(version)
    gate.check(f"{label}: every historical version is restored with its bytes", not lost, lost)

    listing = client.list_object_versions(Bucket="drill-versions")
    markers = {(m["Key"], m["VersionId"]) for m in listing.get("DeleteMarkers", [])}
    gate.check(
        f"{label}: delete markers are restored and still hide the current object",
        all((k, v) in markers for (_, k, v) in state["markers"]),
        sorted(markers),
    )
    try:
        client.head_object(Bucket="drill-versions", Key="ledger")
        hidden = False
    except ClientError:
        hidden = True
    gate.check(f"{label}: a key behind a delete marker still reads as deleted", hidden)

    retention = client.get_object_retention(
        Bucket="drill-locked", Key="retained", VersionId=state["locks"]["retained"]["version"]
    )["Retention"]
    gate.check(
        f"{label}: compliance retention is restored with its mode and date",
        retention["Mode"] == "COMPLIANCE"
        and abs((retention["RetainUntilDate"] - state["locks"]["retained"]["until"]).total_seconds()) < 2,
        {k: str(v) for k, v in retention.items()},
    )
    try:
        client.delete_object(Bucket="drill-locked", Key="retained", VersionId=state["locks"]["retained"]["version"])
        enforced = False
    except ClientError as error:
        enforced = error.response["Error"]["Code"] == "AccessDenied"
    gate.check(f"{label}: restored retention is enforced, not merely reported", enforced)
    hold = client.get_object_legal_hold(Bucket="drill-locked", Key="held", VersionId=state["locks"]["held"]["version"])
    gate.check(f"{label}: legal hold is restored", hold["LegalHold"]["Status"] == "ON")

    outcome = credential_authenticates(server, *state["credential"])
    gate.check(
        f"{label}: the service-account credential still authenticates",
        outcome == "AccessDenied",
        f"S3 answered {outcome}",
    )
    status, body = share_content(server, state["share_token"])
    gate.check(
        f"{label}: an issued share link still serves the object",
        status == 200 and sha256_bytes(body) == state["share_digest"],
        f"HTTP {status}",
    )
    for bucket in ("drill-data", "drill-versions", "drill-locked"):
        listed = set(list_all_keys(client, bucket))
        expected = {k for (b, k) in state["objects"] if b == bucket}
        gate.check(f"{label}: listing {bucket} matches the backed-up catalog", listed == expected,
                   {"missing": sorted(expected - listed)[:5], "extra": sorted(listed - expected)[:5]})
    intact, chain = audit_chain_intact(server)
    gate.check(f"{label}: the restored audit chain verifies", intact, chain)
    for bucket in ("drill-data", "drill-versions", "drill-locked"):
        result = server.api("POST", f"/api/v1/verify/buckets/{bucket}")
        gate.check(f"{label}: integrity verification of {bucket} finds no failure", result.get("failures") == 0, result)


def drop_component(document: dict, component: str) -> dict:
    """Removes one component and its files from a backup manifest."""
    result = dict(document)
    result["components"] = [c for c in document["components"] if c["name"] != component]
    result["files"] = [f for f in document["files"] if not f["path"].startswith(component + "/")]
    return result


def data_directory_untouched(path) -> bool:
    return not any((path / name).exists() for name in ("metadata", "objects", "system"))


def main(gate: Gate) -> None:
    artifact, options = artifact_from_arguments()
    identify_artifact(gate, artifact)
    rng = random.Random(int(options["seed"]))
    large_objects = 4 if options["profile"] == "pr" else 24

    for encrypted in (False, True):
        mode = "encrypted" if encrypted else "plaintext"
        credentials = Secrets()
        root = gate.work_directory / mode
        source = Server(artifact, root / "source", gate.evidence_directory / "logs", credentials=credentials,
                        encrypted=encrypted, name=f"drill-{mode}-source")
        source.start()
        state = populate(gate, source, rng, large_objects)
        events_before = {e["id"] for b in ("drill-data", "drill-versions", "drill-locked") for e in all_events(source, b)}

        backup = root / "backup"
        running = source.cli("server", "backup", str(root / "refused"))
        gate.check(f"{mode}: backup refuses while the server holds the data directory",
                   running.returncode == EXIT_IN_USE, running.stderr.strip()[-300:])
        shutdown = source.stop()
        gate.metric(f"{mode}_shutdown_seconds", shutdown, "s", limit=30.0,
                    basis="server.shutdown_grace_period_seconds default is 30; docs stop with --time 40")

        started = time.monotonic()
        taken = source.cli("server", "backup", str(backup), "--json")
        backup_seconds = time.monotonic() - started
        gate.require(f"{mode}: backup completes", taken.returncode == 0, taken.stderr[-500:])
        gate.metric(f"{mode}_backup_seconds", backup_seconds, "s")
        backup_bytes = sum(p.stat().st_size for p in backup.rglob("*") if p.is_file())
        gate.metric(f"{mode}_backup_bytes", float(backup_bytes), "bytes")
        manifest = json.loads((backup / "backup-manifest.json").read_text())
        gate.context[f"{mode}_manifest_format"] = {k: v for k, v in manifest.items() if not isinstance(v, (list, dict))}

        again = source.cli("server", "backup", str(backup))
        gate.check(f"{mode}: a completed backup is never overwritten", again.returncode == EXIT_CONFLICT, again.stderr[-300:])

        started = time.monotonic()
        verified = source.cli("server", "verify-backup", str(backup), "--level", "full", "--json")
        gate.metric(f"{mode}_verify_full_seconds", time.monotonic() - started, "s")
        gate.require(f"{mode}: full verification of a good backup passes", verified.returncode == 0, verified.stdout[-500:])

        # A write after the backup must not appear in the restore: it is what
        # makes "the recovery point is the stop" a measured fact.
        source.start()
        source.s3().put_object(Bucket="drill-data", Key="after-backup", Body=b"not in the backup")
        source.stop()

        # --- damaged, incomplete, and mismatched backups are refused --------
        damaged = root / "damaged"
        shutil.copytree(backup, damaged)
        victim = next(p for p in sorted((damaged / "objects").rglob("*")) if p.is_file() and p.stat().st_size > 0)
        data = bytearray(victim.read_bytes())
        data[len(data) // 2] ^= 0x01
        victim.write_bytes(bytes(data))
        refused = source.cli("server", "verify-backup", str(damaged), "--level", "checksums")
        gate.check(f"{mode}: checksum verification detects one altered payload byte",
                   refused.returncode == EXIT_UNUSABLE, refused.stdout[-300:])
        target = root / "restore-damaged"
        restore = source.cli("server", "restore", str(damaged), "--level", "full",
                             extra_environment={"RECORD_STORE_STORAGE_DATA_DIRECTORY": str(target)})
        gate.check(f"{mode}: restore refuses a damaged backup before writing anything",
                   restore.returncode == EXIT_UNUSABLE and data_directory_untouched(target), restore.stderr[-300:])

        incomplete = root / "incomplete"
        shutil.copytree(backup, incomplete)
        (incomplete / "INCOMPLETE").write_text("")
        refused = source.cli("server", "verify-backup", str(incomplete), "--level", "manifest")
        gate.check(f"{mode}: a backup marked INCOMPLETE is refused even at manifest level",
                   refused.returncode == EXIT_UNUSABLE, refused.stdout[-300:])

        # A component dropped from the manifest is refused at every level. A
        # component whose files are gone but which the manifest still lists is
        # invisible at `manifest` level by contract ("reads no file contents"),
        # so it has to be caught by `checksums` and by restore.
        unlisted = root / "unlisted-component"
        shutil.copytree(backup, unlisted)
        document = json.loads((unlisted / "backup-manifest.json").read_text())
        before = json.dumps(document)
        document = drop_component(document, "system")
        gate.require("the fixture really removed a component from the manifest", json.dumps(document) != before)
        (unlisted / "backup-manifest.json").write_text(json.dumps(document))
        refused = source.cli("server", "verify-backup", str(unlisted), "--level", "manifest")
        gate.check(f"{mode}: a manifest that omits a required component is refused",
                   refused.returncode == EXIT_UNUSABLE, refused.stdout[-300:])

        missing = root / "missing-component"
        shutil.copytree(backup, missing)
        shutil.rmtree(missing / "system")
        refused = source.cli("server", "verify-backup", str(missing), "--level", "checksums")
        gate.check(f"{mode}: checksum verification refuses a listed component whose files are gone",
                   refused.returncode == EXIT_UNUSABLE, refused.stdout[-300:])
        target = root / "restore-missing"
        restore = source.cli("server", "restore", str(missing), "--level", "checksums",
                             extra_environment={"RECORD_STORE_STORAGE_DATA_DIRECTORY": str(target)})
        gate.check(f"{mode}: restore refuses a backup whose component files are gone, writing nothing",
                   restore.returncode == EXIT_UNUSABLE and data_directory_untouched(target), restore.stderr[-300:])

        # The master key seals credentials, share links and webhook secrets in
        # both modes, so the wrong one is refused in both, not only when
        # payloads are encrypted.
        target = root / "restore-wrong-key"
        wrong = source.cli("server", "restore", str(backup), "--level", "full", extra_environment={
            "RECORD_STORE_STORAGE_DATA_DIRECTORY": str(target),
            "RECORD_STORE_CREDENTIAL_MASTER_KEY": Secrets().master_key,
        })
        gate.check(f"{mode}: restore with the wrong master key is refused before writing anything",
                   wrong.returncode == EXIT_UNUSABLE and data_directory_untouched(target),
                   {"exit": wrong.returncode, "stderr": wrong.stderr.strip()[-300:]})
        leaked = [v for v in credentials.values() if v in wrong.stdout + wrong.stderr]
        gate.check(f"{mode}: the refusal names no key material", not leaked)

        populated = source.cli("server", "restore", str(backup), "--level", "full")
        gate.check(f"{mode}: restore refuses the populated source data directory",
                   populated.returncode != 0, populated.stderr.strip()[-300:])

        # --- an interrupted restore blocks start-up and retries cleanly -----
        target = root / "restore-interrupted"
        caught = False
        environment = source.environment() | {"RECORD_STORE_STORAGE_DATA_DIRECTORY": str(target)}
        for delay in (0.05, 0.1, 0.2, 0.4, 0.8):
            shutil.rmtree(target, ignore_errors=True)
            process = subprocess.Popen([str(artifact.cli), "server", "restore", str(backup), "--level", "checksums"],
                                       env=environment, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            time.sleep(delay)
            if process.poll() is None:
                process.send_signal(signal.SIGKILL)
                process.wait()
                if (target / ".record-store-restore-in-progress").exists():
                    caught = True
                    break
            else:
                process.wait()
        gate.context[f"{mode}_interrupted_restore_caught"] = caught
        if not caught:
            raise InvalidMeasurement(f"{mode}: no kill landed inside a restore; the dataset is too small to interrupt")
        blocked = Server(artifact, target, gate.evidence_directory / "logs", credentials=credentials,
                         encrypted=encrypted, name=f"drill-{mode}-interrupted")
        code, text = blocked.start_expecting_refusal()
        gate.check(f"{mode}: a half-restored data directory refuses to start",
                   code not in (None, 0) and "restore" in text, text.strip()[-300:])
        # Nor is it backed up: the copy would look complete and hold a
        # deployment that never existed.
        half = root / f"backup-of-half-restore"
        refused = subprocess.run([str(artifact.cli), "server", "backup", str(half)], env=environment,
                                 capture_output=True, text=True, timeout=120)
        gate.check(f"{mode}: a half-restored data directory is refused as a backup source (exit 2), writing nothing",
                   refused.returncode == EXIT_CONFIGURATION and not (half / "backup-manifest.json").exists(),
                   {"exit": refused.returncode, "stderr": refused.stderr.strip()[-300:]})

        started = time.monotonic()
        retried = subprocess.run([str(artifact.cli), "server", "restore", str(backup), "--level", "full"],
                                 env=environment, capture_output=True, text=True, timeout=600)
        gate.require(f"{mode}: re-running an interrupted restore succeeds", retried.returncode == 0, retried.stderr[-500:])
        restored = Server(artifact, target, gate.evidence_directory / "logs", credentials=credentials,
                          encrypted=encrypted, name=f"drill-{mode}-restored")
        restored.start()
        read_all(restored.s3(), "drill-data", "small/000")
        recovery_seconds = time.monotonic() - started
        verify_restored(gate, restored, state, f"{mode} restore")
        try:
            restored.s3().head_object(Bucket="drill-data", Key="after-backup")
            after_backup_present = True
        except ClientError:
            after_backup_present = False
        gate.check(f"{mode}: a write made after the backup is not in the restore (recovery point = stop)",
                   not after_backup_present)
        deadline = time.monotonic() + 15
        while True:
            events_after = {e["id"] for b in ("drill-data", "drill-versions", "drill-locked") for e in all_events(restored, b)}
            if events_before <= events_after or time.monotonic() > deadline:
                break
            time.sleep(1)
        gate.check(f"{mode}: the storage-event journal is restored with the same event ids",
                   events_before <= events_after, {"missing": len(events_before - events_after)})
        restored.stop()
        gate.metric(f"{mode}_restore_to_first_verified_read_seconds", recovery_seconds, "s", limit=120.0,
                    provisional=True, enforcement="advisory",
                    basis="provisional RTO for this dataset; calibrate against a production-sized restore")
    gate.context["filesystem"] = filesystem_of(gate.work_directory)


if __name__ == "__main__":
    Gate("REC-BACKUP", "backups restore into an equivalent deployment; bad ones are refused").run(main)
