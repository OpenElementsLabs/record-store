#!/usr/bin/env python3
"""SEC-REDACT: no secret the process was given, or issued, appears where it must not.

Secrets under test, all unique to this run:

  * the root secret key, the credential master key and the management token,
    which arrive through the environment;
  * a service account's secret access key, which the server issues once in the
    creation response and must never repeat.

Surfaces scanned: the server's JSON log for the whole run (start-up, requests
that fail authentication, a request made with a wrong secret, shutdown), every
operational endpoint, the audit and event APIs, `check-config`, `doctor` and
`backup --json` output, and -- byte for byte -- every file in the data
directory and in a backup. docs/operations/backup-and-restore.md states the
master key, root credentials and tokens are in neither.

The one place a secret may legitimately appear is the single creation response
that issues it; that response is excluded, and only that.
"""

from __future__ import annotations

import json
import urllib.request

from botocore.exceptions import ClientError

from gatelib import (
    Gate,
    Secrets,
    Server,
    artifact_from_arguments,
    identify_artifact,
)


def fetch(url: str, token: str | None = None) -> str:
    request = urllib.request.Request(url)
    if token:
        request.add_header("authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return response.read().decode(errors="replace")
    except urllib.error.HTTPError as error:
        return error.read().decode(errors="replace")


def main(gate: Gate) -> None:
    artifact, _ = artifact_from_arguments()
    identify_artifact(gate, artifact)
    credentials = Secrets()
    scrape_token = Secrets().system_token
    server = Server(
        artifact,
        gate.work_directory / "data",
        gate.evidence_directory / "logs",
        credentials=credentials,
        encrypted=True,
        name="redaction",
        extra_environment={
            "RECORD_STORE_METRICS_SCRAPE_TOKEN": scrape_token,
            "RECORD_STORE_LOG": "record_store=debug,tower_http=debug",
        },
    )
    server.start()
    captured: dict[str, str] = {}

    client = server.s3()
    client.create_bucket(Bucket="redaction")
    client.put_object(Bucket="redaction", Key="k", Body=b"payload")
    issued = server.api("POST", "/api/v1/service-accounts", {"name": "redaction-probe"})
    issued_secret = issued["secret_access_key"]
    key_id = issued["credential"]["key_id"]
    try:
        server.s3(key_id, issued_secret).list_objects_v2(Bucket="redaction")
    except ClientError:
        pass
    try:
        server.s3(credentials.root_access_key, credentials.root_secret_key[::-1]).list_buckets()
    except ClientError as error:
        captured["s3 error for a wrong secret"] = json.dumps(error.response, default=str)
    captured["management answer to a wrong token"] = str(server.api_raw("GET", "/api/v1/buckets", token="0" * 64))
    captured["management answer to the root secret as a token"] = str(
        server.api_raw("GET", "/api/v1/buckets", token=credentials.root_secret_key))
    for path in ("/health", "/ready"):
        captured[path] = fetch(server.api_endpoint + path)
    captured["/metrics"] = fetch(server.api_endpoint + "/metrics", scrape_token)
    for path in ("/api/v1/system/info", "/api/v1/system/metrics", "/api/v1/service-accounts",
                 "/api/v1/audit/events?limit=1000", "/api/v1/events?limit=1000", "/api/v1/storage/status"):
        captured[path] = json.dumps(server.api("GET", path))
    for command in (("server", "check-config", "--json"), ("server", "doctor", "--json")):
        result = server.cli(*command)
        captured[" ".join(command)] = result.stdout + result.stderr
    server.stop()
    backup = gate.work_directory / "backup"
    result = server.cli("server", "backup", str(backup), "--json")
    captured["server backup --json"] = result.stdout + result.stderr
    gate.require("the backup used for the scan completes", result.returncode == 0, result.stderr[-300:])
    captured["server log"] = server.log_path.read_text(errors="replace")

    secrets = {
        "root secret key": credentials.root_secret_key,
        "credential master key": credentials.master_key,
        "management system token": credentials.system_token,
        "metrics scrape token": scrape_token,
        "issued service-account secret": issued_secret,
    }
    for surface, text in sorted(captured.items()):
        leaked = [name for name, value in secrets.items() if value in text]
        gate.check(f"no secret appears in {surface}", not leaked, leaked)

    for label, root in (("data directory", server.data_directory), ("backup", backup)):
        leaks: list[str] = []
        scanned = 0
        for path in sorted(root.rglob("*")):
            if not path.is_file():
                continue
            scanned += 1
            content = path.read_bytes()
            for name, value in secrets.items():
                if value.encode() in content:
                    leaks.append(f"{name} in {path.relative_to(root)}")
        gate.context[f"{label}_files_scanned"] = scanned
        gate.check(f"no secret is stored anywhere in the {label} ({scanned} files, byte for byte)", not leaks, leaks)
    gate.context["surfaces"] = sorted(captured)


if __name__ == "__main__":
    Gate("SEC-REDACT", "secrets never reach logs, APIs, CLI output, data or backups").run(main)
