#!/usr/bin/env python3
"""CMP-UPGRADE-CONTAINER: the upgrade guide works, as written, with real images.

CMP-UPGRADE proves the upgrade path with the binaries. This gate proves the
*document*: it runs the published 0.1.3 image, then follows
docs/deployment/upgrading.md with an image built from this commit -- stop with
the drain window, back up with the new image through --volumes-from, verify
the backup, check the configuration, start, verify the data -- and then the
documented rollback: move the data aside, restore the pre-upgrade backup with
the new image, start the previous image, and verify the pre-upgrade state.

It also refuses to drift from the guide: every command form it runs must still
appear in upgrading.md, so a change to one without the other fails here.

Needs Docker, a network able to pull ghcr.io/openelementslabs/record-store:0.1.3,
and passwordless sudo to give the bind mounts to uid 10001 (GitHub-hosted
runners have all three). Without Docker it is an infrastructure error, never a
pass.
"""

from __future__ import annotations

import hashlib
import json
import os
import secrets
import shutil
import subprocess
import time
import urllib.error
import urllib.request
from pathlib import Path

from gatelib import REPOSITORY_ROOT, Gate, InfrastructureError

PREVIOUS_IMAGE = os.environ.get("GATE_PREVIOUS_IMAGE", "ghcr.io/openelementslabs/record-store:0.1.3")
CANDIDATE_IMAGE = "record-store-gate:candidate"
API_PORT = 47701
S3_PORT = 47700
UID = "10001"

# The command forms this gate runs, as they must appear in the guide.
DOCUMENTED = [
    "docker stop --time 40 record-store",
    '--volumes-from record-store --volume /backups:/backups',
    'backup /backups/pre-upgrade',
    'verify-backup /backups/pre-upgrade --level full',
    '"$NEW" check-config',
    '--volumes-from record-store --env-file /etc/record-store/env "$NEW" doctor',
    "mv /var/lib/record-store /var/lib/record-store.failed",
    "chown 10001:10001 /var/lib/record-store",
    "restore /backups/pre-upgrade --level full",
]


def docker(*arguments: str, check: bool = True, timeout: float = 900) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(["docker", *arguments], capture_output=True, text=True, timeout=timeout)
    if check and result.returncode != 0:
        raise InfrastructureError(f"docker {' '.join(arguments[:3])} failed: {result.stderr.strip()[-400:]}")
    return result


def owned_directory(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True)
    subprocess.run(["sudo", "chown", f"{UID}:{UID}", str(path)], check=True)


def api(token: str, method: str, path: str, body: bytes | None = None, content_type: str | None = None) -> tuple[int, bytes]:
    request = urllib.request.Request(f"http://127.0.0.1:{API_PORT}{path}", data=body, method=method)
    request.add_header("authorization", f"Bearer {token}")
    if content_type:
        request.add_header("content-type", content_type)
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.read()


def wait_ready(name: str, timeout: float = 90) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        state = docker("inspect", "--format", "{{.State.Running}}", name, check=False).stdout.strip()
        if state == "false":
            return False
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{API_PORT}/ready", timeout=2) as response:
                if response.status == 200:
                    return True
        except (urllib.error.URLError, OSError):
            pass
        time.sleep(1)
    return False


def run_server(name: str, image: str, data: Path, env_file: Path) -> None:
    docker("run", "--detach", "--name", name, "--env-file", str(env_file),
           "--volume", f"{data}:/var/lib/record-store",
           "--publish", f"127.0.0.1:{S3_PORT}:7600", "--publish", f"127.0.0.1:{API_PORT}:7601", image)


def main(gate: Gate) -> None:
    if shutil.which("docker") is None or docker("info", check=False).returncode != 0:
        raise InfrastructureError("Docker is not available")
    guide = (REPOSITORY_ROOT / "docs/deployment/upgrading.md").read_text()
    for form in DOCUMENTED:
        gate.check(f"the upgrade guide still documents: {form}", form in guide)

    root = gate.work_directory
    data, backups = root / "var-lib-record-store", root / "backups"
    owned_directory(data)
    owned_directory(backups)
    token = secrets.token_hex(32)
    env_file = root / "env"
    env_file.write_text("\n".join([
        f"RECORD_STORE_ROOT_ACCESS_KEY=gate-{secrets.token_hex(6)}",
        f"RECORD_STORE_ROOT_SECRET_KEY={secrets.token_hex(24)}",
        f"RECORD_STORE_CREDENTIAL_MASTER_KEY={secrets.token_hex(32)}",
        f"RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN={token}",
        "RECORD_STORE_STORAGE_ENCRYPTION_ENABLED=true",
    ]) + "\n")
    logs = gate.evidence_directory / "logs"
    logs.mkdir(parents=True, exist_ok=True)

    docker("pull", PREVIOUS_IMAGE)
    commit = subprocess.run(["git", "-C", str(REPOSITORY_ROOT), "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
    built = docker("build", "--file", "deploy/docker/Dockerfile", "--build-arg", f"RECORD_STORE_BUILD_COMMIT={commit}",
                   "--tag", CANDIDATE_IMAGE, str(REPOSITORY_ROOT), timeout=3600)
    gate.context["images"] = {
        "previous": docker("image", "inspect", PREVIOUS_IMAGE, "--format", "{{.Id}}").stdout.strip(),
        "candidate": docker("image", "inspect", CANDIDATE_IMAGE, "--format", "{{.Id}}").stdout.strip(),
    }
    (logs / "build.log").write_text(built.stdout[-20000:] + built.stderr[-20000:])
    containers = ["record-store", "record-store-new", "record-store-rollback", "record-store-downgrade"]
    try:
        for name in containers:
            docker("rm", "--force", name, check=False)

        # --- 0.1.3, running as operators run it --------------------------------
        run_server("record-store", PREVIOUS_IMAGE, data, env_file)
        gate.require("0.1.3 starts from its published image", wait_ready("record-store"))
        payload = secrets.token_bytes(1 << 20)
        digest = hashlib.sha256(payload).hexdigest()
        gate.require("0.1.3 creates a bucket", api(token, "POST", "/api/v1/buckets", json.dumps({"name": "upgrade"}).encode(), "application/json")[0] in (200, 201))
        status, _ = api(token, "PUT", "/api/v1/buckets/upgrade/object/before.bin", payload, "application/octet-stream")
        gate.require("0.1.3 stores an object", status in (200, 201), status)

        # --- the upgrade, step by step as documented ----------------------------
        started = time.monotonic()
        docker("stop", "--time", "40", "record-store")
        exit_code = docker("inspect", "--format", "{{.State.ExitCode}}", "record-store").stdout.strip()
        gate.check("step 1: 0.1.3 stops cleanly within the drain window",
                   exit_code == "0" and time.monotonic() - started < 40, {"exit_code": exit_code})
        backup = docker("run", "--rm", "--volumes-from", "record-store", "--volume", f"{backups}:/backups",
                        "--env-file", str(env_file), CANDIDATE_IMAGE, "backup", "/backups/pre-upgrade", check=False)
        gate.require("step 3: the new image backs up the stopped deployment through --volumes-from",
                     backup.returncode == 0, (backup.stdout + backup.stderr)[-500:])
        verify = docker("run", "--rm", "--volume", f"{backups}:/backups", "--env-file", str(env_file),
                        CANDIDATE_IMAGE, "verify-backup", "/backups/pre-upgrade", "--level", "full", check=False)
        gate.check("step 3: the backup verifies at full level", verify.returncode == 0, (verify.stdout + verify.stderr)[-500:])
        checked = docker("run", "--rm", "--env-file", str(env_file), CANDIDATE_IMAGE, "check-config", check=False)
        gate.check("step 4: check-config runs and passes as documented", checked.returncode == 0,
                   (checked.stdout + checked.stderr)[-500:])
        doctor = docker("run", "--rm", "--volumes-from", "record-store", "--env-file", str(env_file),
                        CANDIDATE_IMAGE, "doctor", check=False)
        gate.check("step 5: doctor finds nothing that would stop the new image starting on the 0.1.3 data",
                   doctor.returncode == 0, (doctor.stdout + doctor.stderr)[-800:])
        run_server("record-store-new", CANDIDATE_IMAGE, data, env_file)
        gate.require("step 6: the new image starts on the 0.1.3 data", wait_ready("record-store-new"))
        status, body = api(token, "GET", "/api/v1/buckets/upgrade/object-content/before.bin")
        gate.check("step 7: the object written by 0.1.3 reads back byte for byte",
                   status == 200 and hashlib.sha256(body).hexdigest() == digest, status)
        status, _ = api(token, "PUT", "/api/v1/buckets/upgrade/object/after.bin", b"written after upgrade", "application/octet-stream")
        gate.check("the upgraded deployment accepts writes", status in (200, 201), status)
        docker("stop", "--time", "40", "record-store-new")

        # --- the downgrade the guide warns about --------------------------------
        docker("run", "--detach", "--name", "record-store-downgrade", "--env-file", str(env_file),
               "--volume", f"{data}:/var/lib/record-store", PREVIOUS_IMAGE)
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline and docker("inspect", "--format", "{{.State.Running}}", "record-store-downgrade").stdout.strip() == "true":
            time.sleep(1)
        downgrade_exit = docker("inspect", "--format", "{{.State.ExitCode}}", "record-store-downgrade").stdout.strip()
        downgrade_log = docker("logs", "record-store-downgrade", check=False)
        (logs / "downgrade.log").write_text(downgrade_log.stdout + downgrade_log.stderr)
        gate.check("the previous image refuses the upgraded data directory, as the guide says",
                   downgrade_exit not in ("", "0"), {"exit_code": downgrade_exit})

        # --- the documented rollback ---------------------------------------------
        failed = data.with_name(data.name + ".failed")
        subprocess.run(["sudo", "mv", str(data), str(failed)], check=True)
        owned_directory(data)
        restore = docker("run", "--rm", "--volume", f"{data}:/var/lib/record-store", "--volume", f"{backups}:/backups",
                         "--env-file", str(env_file), CANDIDATE_IMAGE, "restore", "/backups/pre-upgrade", "--level", "full", check=False)
        gate.require("rollback: the pre-upgrade backup restores into the empty directory",
                     restore.returncode == 0, (restore.stdout + restore.stderr)[-500:])
        run_server("record-store-rollback", PREVIOUS_IMAGE, data, env_file)
        gate.require("rollback: the previous image starts on the restored data", wait_ready("record-store-rollback"))
        status, body = api(token, "GET", "/api/v1/buckets/upgrade/object-content/before.bin")
        gate.check("rollback: the pre-upgrade object reads back byte for byte",
                   status == 200 and hashlib.sha256(body).hexdigest() == digest, status)
        status, _ = api(token, "GET", "/api/v1/buckets/upgrade/object-content/after.bin")
        gate.check("rollback: the write made after the upgrade is absent (recovery point = backup)", status == 404, status)
    finally:
        for name in containers:
            output = docker("logs", name, check=False)
            if output.returncode == 0:
                (logs / f"{name}.log").write_text(output.stdout + output.stderr)
            docker("rm", "--force", name, check=False)
        subprocess.run(["sudo", "rm", "-rf", str(root / "var-lib-record-store"), str(root / "var-lib-record-store.failed"),
                        str(root / "backups")], check=False)


if __name__ == "__main__":
    Gate("CMP-UPGRADE-CONTAINER", "the upgrade guide works as written with real images").run(main)
