#!/usr/bin/env python3
"""CMP-UPGRADE: data written by the previous release is served correctly after upgrade.

The supported path into this release is from 0.1.3 (CHANGELOG: 0.1.3 converts
every redb file to format v3; this release cannot read v2). This gate runs the
real 0.1.3 binaries -- built from the v0.1.3 tag or extracted from its
published image, never simulated -- to write a data directory with every kind
of state 0.1.3 had, and then follows docs/deployment/upgrading.md with the
candidate:

  1. back up the stopped 0.1.3 deployment and verify the backup;
  2. validate the old configuration with the new binary (`check-config`);
  3. start the candidate on the same data directory and check everything the
     previous release served is still served, byte for byte, still protected;
  4. confirm the downgrade boundary: 0.1.3 refuses to serve the upgraded
     directory and leaves it byte-identical (its message is recorded, not
     asserted -- that text belongs to a release already published);
  5. roll back the documented way -- restore the pre-upgrade backup into an
     empty directory -- and confirm 0.1.3 serves the pre-upgrade state again.

0.1.3 ships no `server backup`/`restore`, so steps 1 and 5 necessarily use the
candidate's CLI. Step 1 therefore also asserts that taking the backup leaves
the 0.1.3 data directory byte-identical: a backup that migrated its source
would destroy the rollback it exists to provide.
"""

from __future__ import annotations

import hashlib
import random
import shutil
import subprocess
import urllib.request
from pathlib import Path

from botocore.exceptions import ClientError

from gatelib import (
    Artifact,
    Gate,
    InfrastructureError,
    Secrets,
    Server,
    artifact_from_arguments,
    audit_chain_intact,
    identify_artifact,
    list_all_keys,
    read_all,
    sha256_bytes,
)

MIB = 1024 * 1024
PREVIOUS_VERSION = "0.1.3"


def tree_digest(root: Path) -> dict[str, str]:
    return {
        str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
        for path in sorted(root.rglob("*"))
        if path.is_file()
    }


def populate(server: Server, rng: random.Random) -> dict:
    client = server.s3()
    state: dict = {"objects": {}, "versions": {}, "markers": []}
    client.create_bucket(Bucket="upgrade-data")
    for index in range(50):
        body = rng.randbytes(rng.randint(0, 96 * 1024))
        client.put_object(Bucket="upgrade-data", Key=f"small/{index:03d}", Body=body, Metadata={"index": str(index)})
        state["objects"][("upgrade-data", f"small/{index:03d}")] = sha256_bytes(body)
    body = rng.randbytes(12 * MIB)
    client.put_object(Bucket="upgrade-data", Key="large", Body=body, ContentType="application/x-upgrade")
    state["objects"][("upgrade-data", "large")] = sha256_bytes(body)
    upload = client.create_multipart_upload(Bucket="upgrade-data", Key="multipart")
    parts, whole = [], b""
    for number in (1, 2):
        piece = rng.randbytes(5 * MIB if number == 1 else 700_000)
        whole += piece
        etag = client.upload_part(Bucket="upgrade-data", Key="multipart", UploadId=upload["UploadId"],
                                  PartNumber=number, Body=piece)["ETag"]
        parts.append({"PartNumber": number, "ETag": etag})
    client.complete_multipart_upload(Bucket="upgrade-data", Key="multipart", UploadId=upload["UploadId"],
                                     MultipartUpload={"Parts": parts})
    state["objects"][("upgrade-data", "multipart")] = sha256_bytes(whole)
    state["multipart_etag"] = client.head_object(Bucket="upgrade-data", Key="multipart")["ETag"]

    open_upload = client.create_multipart_upload(Bucket="upgrade-data", Key="unfinished")["UploadId"]
    part = rng.randbytes(5 * MIB)
    etag = client.upload_part(Bucket="upgrade-data", Key="unfinished", UploadId=open_upload, PartNumber=1, Body=part)["ETag"]
    state["open_upload"] = (open_upload, etag, part)

    client.create_bucket(Bucket="upgrade-versions")
    client.put_bucket_versioning(Bucket="upgrade-versions", VersioningConfiguration={"Status": "Enabled"})
    for key in ("a", "b"):
        for _ in range(3):
            body = rng.randbytes(rng.randint(1, 32 * 1024))
            version = client.put_object(Bucket="upgrade-versions", Key=key, Body=body)["VersionId"]
            state["versions"][("upgrade-versions", key, version)] = sha256_bytes(body)
    state["markers"].append(client.delete_object(Bucket="upgrade-versions", Key="b")["VersionId"])
    state["objects"][("upgrade-versions", "a")] = [d for (_, k, _), d in state["versions"].items() if k == "a"][-1]

    account = server.api("POST", "/api/v1/service-accounts", {"name": "upgrade-drill"})
    state["credential"] = (account["credential"]["key_id"], account["secret_access_key"])
    share = server.api("POST", "/api/v1/buckets/upgrade-data/object-shares/small/002", {"label": "upgrade"})
    state["share_token"] = share["url"].rsplit("/", 1)[-1]
    rule = server.api("POST", "/api/v1/buckets/upgrade-data/lifecycle", {"prefix": "tmp/", "expiration": 30})
    state["lifecycle_rule"] = rule
    return state


def verify_state(gate: Gate, server: Server, state: dict, label: str, *, candidate: bool) -> None:
    client = server.s3()
    wrong = []
    for (bucket, key), digest in sorted(state["objects"].items()):
        try:
            if sha256_bytes(read_all(client, bucket, key)) != digest:
                wrong.append(key)
        except Exception as error:  # noqa: BLE001
            wrong.append(f"{key}: {type(error).__name__}")
    gate.check(f"{label}: every object written by {PREVIOUS_VERSION} reads back with its bytes", not wrong, wrong[:10])
    lost = []
    for (bucket, key, version), digest in sorted(state["versions"].items()):
        try:
            if sha256_bytes(read_all(client, bucket, key, VersionId=version)) != digest:
                lost.append(version)
        except Exception:  # noqa: BLE001
            lost.append(version)
    gate.check(f"{label}: every historical version reads back with its bytes", not lost, lost)
    markers = {m["VersionId"] for m in client.list_object_versions(Bucket="upgrade-versions").get("DeleteMarkers", [])}
    gate.check(f"{label}: delete markers survive", set(state["markers"]) <= markers)
    head = client.head_object(Bucket="upgrade-data", Key="small/007")
    gate.check(f"{label}: user metadata survives", head.get("Metadata", {}).get("index") == "7", head.get("Metadata"))
    gate.check(f"{label}: content type survives",
               client.head_object(Bucket="upgrade-data", Key="large")["ContentType"] == "application/x-upgrade")
    gate.check(f"{label}: a multipart object keeps its ETag",
               client.head_object(Bucket="upgrade-data", Key="multipart")["ETag"] == state["multipart_etag"])
    upload_id, etag, _ = state["open_upload"]
    parts = client.list_parts(Bucket="upgrade-data", Key="unfinished", UploadId=upload_id).get("Parts", [])
    gate.check(f"{label}: an unfinished multipart upload keeps its parts",
               [p["ETag"] for p in parts] == [etag], [p.get("ETag") for p in parts])
    for bucket in ("upgrade-data", "upgrade-versions"):
        listed = set(list_all_keys(client, bucket))
        expected = {k for (b, k) in state["objects"] if b == bucket}
        gate.check(f"{label}: listing {bucket} matches", listed == expected,
                   {"missing": sorted(expected - listed)[:5], "extra": sorted(listed - expected)[:5]})
    try:
        server.s3(*state["credential"]).list_objects_v2(Bucket="upgrade-data")
        outcome = "allowed"
    except ClientError as error:
        outcome = error.response["Error"]["Code"]
    gate.check(f"{label}: a service-account credential issued by {PREVIOUS_VERSION} still authenticates",
               outcome == "AccessDenied", f"S3 answered {outcome}")
    try:
        with urllib.request.urlopen(f"{server.api_endpoint}/s/{state['share_token']}/content?download=true", timeout=30) as response:
            status, body = response.status, response.read()
    except urllib.error.HTTPError as error:
        status, body = error.code, b""
    gate.check(f"{label}: a share link issued by {PREVIOUS_VERSION} still serves its object",
               status == 200 and sha256_bytes(body) == state["objects"][("upgrade-data", "small/002")], f"HTTP {status}")
    rules = server.api("GET", "/api/v1/buckets/upgrade-data/lifecycle")
    rule_list = rules if isinstance(rules, list) else rules.get("rules", [])
    gate.check(f"{label}: lifecycle rules survive", any(r.get("prefix") == "tmp/" for r in rule_list), rules)
    if candidate:
        intact, chain = audit_chain_intact(server)
        gate.check(f"{label}: the audit chain verifies", intact, chain)
        inspection = server.api("GET", "/api/v1/storage/inspect?maximum_entries=1000000")
        gate.check(f"{label}: storage inspection is clean",
                   inspection.get("metadata_without_data") == 0 and inspection.get("data_without_metadata") == 0,
                   inspection)
        for bucket in ("upgrade-data", "upgrade-versions"):
            result = server.api("POST", f"/api/v1/verify/buckets/{bucket}")
            gate.check(f"{label}: integrity verification of {bucket} finds no failure", result.get("failures") == 0, result)


def main(gate: Gate) -> None:
    artifact, options = artifact_from_arguments()
    identify_artifact(gate, artifact)
    if not options["previous_bin_dir"]:
        raise InfrastructureError("--previous-bin-dir is required: the gate needs the real previous release")
    previous = Artifact(Path(options["previous_bin_dir"]).resolve())
    described = previous.describe()
    gate.context["previous_artifact"] = described
    if described["reported_version"] != f"record-store {PREVIOUS_VERSION}":
        raise InfrastructureError(f"previous binaries report {described['reported_version']!r}")
    rng = random.Random(int(options["seed"]))
    # 0.1.3's own example configuration: shipped next to its binaries by
    # run-stage.sh and the CI job that builds them, or read from the tag.
    config_file = gate.work_directory / "previous-example.toml"
    shipped = previous.directory.parent / "record-store.example.toml"
    if shipped.is_file():
        config_file.write_bytes(shipped.read_bytes())
    else:
        shown = subprocess.run(["git", "-C", str(Path(__file__).resolve().parents[2]), "show",
                                f"v{PREVIOUS_VERSION}:record-store.example.toml"], capture_output=True)
        if shown.returncode != 0:
            raise InfrastructureError(f"cannot obtain {PREVIOUS_VERSION}'s record-store.example.toml "
                                      "(no copy beside its binaries, and the tag is not fetched)")
        config_file.write_bytes(shown.stdout)

    for encrypted in (False, True):
        mode = "encrypted" if encrypted else "plaintext"
        credentials = Secrets()
        data = gate.work_directory / mode / "data"
        logs = gate.evidence_directory / "logs"
        old = Server(previous, data, logs, credentials=credentials, encrypted=encrypted,
                     name=f"upgrade-{mode}-{PREVIOUS_VERSION}", expected_version=PREVIOUS_VERSION)
        old.start()
        state = populate(old, rng)
        verify_state(gate, old, state, f"{mode} on {PREVIOUS_VERSION}", candidate=False)
        old.stop()

        # 1. Pre-upgrade backup, taken the only way the docs allow on 0.1.3.
        before = tree_digest(data)
        new = Server(artifact, data, logs, credentials=credentials, encrypted=encrypted, name=f"upgrade-{mode}-candidate")
        backup = gate.work_directory / mode / "pre-upgrade"
        taken = new.cli("server", "backup", str(backup))
        gate.require(f"{mode}: the candidate CLI backs up a stopped {PREVIOUS_VERSION} deployment",
                     taken.returncode == 0, taken.stderr[-500:])
        gate.check(f"{mode}: taking the backup leaves the {PREVIOUS_VERSION} data directory byte-identical",
                   tree_digest(data) == before,
                   sorted(k for k in set(before) | set(tree_digest(data)) if before.get(k) != tree_digest(data).get(k))[:10])
        verified = new.cli("server", "verify-backup", str(backup), "--level", "full")
        gate.require(f"{mode}: the pre-upgrade backup verifies at full level", verified.returncode == 0, verified.stdout[-500:])

        # 2. The old configuration is still valid.
        checked = new.cli("server", "--config", str(config_file), "check-config",
                          extra_environment={"RECORD_STORE_STORAGE_DATA_DIRECTORY": str(data)})
        gate.check(f"{mode}: {PREVIOUS_VERSION}'s example configuration passes the candidate's check-config",
                   checked.returncode == 0, (checked.stdout + checked.stderr)[-500:])

        # 3. Upgrade in place.
        startup = new.start()
        gate.metric(f"{mode}_upgrade_first_start_seconds", startup, "s", limit=5.0,
                    basis="container start period in deploy/docker/Dockerfile HEALTHCHECK is 5 s")
        verify_state(gate, new, state, f"{mode} after upgrade", candidate=True)
        new.s3().put_object(Bucket="upgrade-data", Key="written-after-upgrade", Body=b"new")
        gate.check(f"{mode}: the upgraded deployment accepts writes",
                   read_all(new.s3(), "upgrade-data", "written-after-upgrade") == b"new")
        new.stop()

        # 4. Downgrade is refused, explicitly, and harmlessly.
        upgraded = tree_digest(data)
        code, text = old.start_expecting_refusal()
        gate.check(f"{mode}: {PREVIOUS_VERSION} refuses the upgraded data directory",
                   code not in (None, 0), text.strip()[-400:])
        # What the refusal *says* is decided by the already-published 0.1.3,
        # which this release cannot change; what this release controls is that
        # the refusal happens, harms nothing, and is documented. The text is
        # kept as evidence for the upgrade notes.
        explained = "newer" in text.lower() or "schema" in text.lower()
        gate.context[f"{mode}_downgrade_refusal"] = {"exit_code": code, "explains_schema": explained,
                                                    "tail": text.strip()[-600:]}
        if not explained:
            gate.note(f"{mode}: {PREVIOUS_VERSION} refuses upgraded data by panicking, without naming the schema")
        gate.check(f"{mode}: the refused downgrade changed nothing on disk", tree_digest(data) == upgraded)

        # 5. The documented rollback: restore the pre-upgrade backup, run the old binary.
        shutil.move(str(data), str(data.with_name("data.failed")))
        restored = new.cli("server", "restore", str(backup), "--level", "full")
        gate.require(f"{mode}: the pre-upgrade backup restores into an empty directory",
                     restored.returncode == 0, restored.stderr[-500:])
        rolled_back = Server(previous, data, logs, credentials=credentials, encrypted=encrypted,
                             name=f"upgrade-{mode}-rollback", expected_version=PREVIOUS_VERSION)
        rolled_back.start()
        verify_state(gate, rolled_back, state, f"{mode} rolled back to {PREVIOUS_VERSION}", candidate=False)
        try:
            rolled_back.s3().head_object(Bucket="upgrade-data", Key="written-after-upgrade")
            present = True
        except ClientError:
            present = False
        gate.check(f"{mode}: the rollback returns to the pre-upgrade recovery point", not present)
        rolled_back.stop()


if __name__ == "__main__":
    Gate("CMP-UPGRADE", f"data written by {PREVIOUS_VERSION} survives upgrade; rollback path works").run(main)
