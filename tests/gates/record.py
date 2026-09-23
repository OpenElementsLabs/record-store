#!/usr/bin/env python3
"""Runs one gate from the matrix and writes its result record.

    python3 tests/gates/record.py --gate REC-CRASH --candidate out/candidate \\
        --results out/results --stage integration

The command comes from the matrix, not from the caller, so a workflow cannot
run something weaker under a gate's name. The record binds the outcome to:

  * the commit and the gate's definition digest (its matrix entry plus the
    files it lists as its implementation), so a later change to the gate
    invalidates the evidence;
  * the candidate's binary digests, for artifact-bound gates, cross-checked
    against the digests the gate itself observed -- a gate that ran some other
    binary is recorded as an invalid measurement, never as a pass;
  * the environment it ran in.

Outcomes are classified from the exit code (see gatelib.py). Only
infrastructure errors are retried, at most `infra_retries` times, and every
attempt is kept in the record: a retry is visible, never a clean slate. A
product failure is never retried. A timeout is a failure, not an
infrastructure error -- a hang is a behaviour.
"""

from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import os
import platform
import subprocess
import sys
import time
import tomllib
from pathlib import Path

REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
OUTCOMES = {0: "pass", 1: "fail", 65: "invalid_measurement", 75: "infrastructure_error", 77: "skipped"}


def load_matrix(path: Path) -> dict:
    with open(path, "rb") as handle:
        return tomllib.load(handle)


def gate_definition(matrix: dict, gate_id: str) -> dict:
    for gate in matrix["gate"]:
        if gate["id"] == gate_id:
            return gate
    raise SystemExit(f"no gate {gate_id!r} in the matrix")


def definition_digest(gate: dict, root: Path = REPOSITORY_ROOT) -> str:
    """Digest of what the gate is: its definition and its implementation files."""
    digest = hashlib.sha256(json.dumps(gate, sort_keys=True, default=str).encode())
    for relative in sorted(gate.get("implementation", [])):
        path = root / relative
        digest.update(relative.encode())
        digest.update(path.read_bytes() if path.is_file() else b"<missing>")
    return digest.hexdigest()


def git(*arguments: str) -> str:
    return subprocess.run(["git", "-C", str(REPOSITORY_ROOT), *arguments], capture_output=True, text=True).stdout.strip()


def kill_group(process: subprocess.Popen) -> None:
    """Kills the gate and everything it started (servers included)."""
    import signal

    try:
        os.killpg(process.pid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError):
        pass
    process.wait()


def utc_now() -> str:
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--gate", required=True)
    parser.add_argument("--matrix", type=Path, default=REPOSITORY_ROOT / "release/gates.toml")
    parser.add_argument("--candidate", type=Path, help="directory holding candidate.json and bin/")
    parser.add_argument("--previous", type=Path, help="directory holding the previous release's bin/")
    parser.add_argument("--results", type=Path, required=True)
    parser.add_argument("--stage", required=True, choices=["pr", "integration", "scheduled", "candidate"])
    parser.add_argument("--profile", help="workload profile passed to the gate (default: from the stage)")
    parser.add_argument("--seed", default=os.environ.get("GATE_SEED", "20260923"))
    arguments = parser.parse_args()

    matrix = load_matrix(arguments.matrix)
    gate = gate_definition(matrix, arguments.gate)
    if arguments.stage not in gate["stages"]:
        print(f"{gate['id']} is not part of the {arguments.stage} stage", file=sys.stderr)
        return 2
    candidate: dict = {}
    if gate.get("binds_artifact"):
        if not arguments.candidate or not (arguments.candidate / "candidate.json").is_file():
            print(f"{gate['id']} is artifact-bound and needs --candidate with a candidate.json", file=sys.stderr)
            return 2
        candidate = json.loads((arguments.candidate / "candidate.json").read_text())
    elif arguments.candidate and (arguments.candidate / "candidate.json").is_file():
        candidate = json.loads((arguments.candidate / "candidate.json").read_text())

    profile = arguments.profile or {"pr": "pr", "integration": "pr", "scheduled": "scheduled",
                                    "candidate": "candidate"}[arguments.stage]
    results = arguments.results.resolve()
    evidence_root = results / "evidence" / gate["id"]
    environment = dict(os.environ)
    environment.update({
        "CANDIDATE": str(arguments.candidate.resolve()) if arguments.candidate else "",
        "PREVIOUS": str(arguments.previous.resolve()) if arguments.previous else "",
        "PROFILE": profile,
        "SEED": str(arguments.seed),
        "GATE_COMMIT": git("rev-parse", "HEAD"),
    })
    # The gate scripts import gatelib from their own directory.
    environment["PYTHONPATH"] = str(REPOSITORY_ROOT / "tests/gates") + os.pathsep + environment.get("PYTHONPATH", "")

    attempts = []
    started_at = utc_now()
    limit = 1 + int(gate.get("infra_retries", 0))
    detail: dict = {}
    for attempt in range(1, limit + 1):
        evidence = evidence_root / f"attempt-{attempt}"
        evidence.mkdir(parents=True, exist_ok=True)
        detail_path = evidence / "gate-detail.json"
        environment["GATE_EVIDENCE_DIR"] = str(evidence)
        environment["GATE_DETAIL"] = str(detail_path)
        log_path = evidence / "output.log"
        began = time.monotonic()
        timed_out = False
        print(f"--- {gate['id']} attempt {attempt}/{limit}: {gate['command']}", flush=True)
        with open(log_path, "wb") as log:
            process = subprocess.Popen(["bash", "-o", "pipefail", "-c", gate["command"]], cwd=REPOSITORY_ROOT,
                                       env=environment, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                       start_new_session=True)
            try:
                deadline = began + 60 * float(gate["timeout_minutes"])
                assert process.stdout is not None
                os.set_blocking(process.stdout.fileno(), False)
                while process.poll() is None:
                    chunk = process.stdout.read()
                    if chunk:
                        log.write(chunk)
                        sys.stdout.buffer.write(chunk)
                        sys.stdout.flush()
                    if time.monotonic() > deadline:
                        timed_out = True
                        kill_group(process)
                        process.wait()
                        break
                    time.sleep(0.05)
                rest = process.stdout.read() or b""
                log.write(rest)
                sys.stdout.buffer.write(rest)
            finally:
                kill_group(process)
        elapsed = time.monotonic() - began
        outcome = "fail" if timed_out else OUTCOMES.get(process.returncode, "fail")
        detail = json.loads(detail_path.read_text()) if detail_path.is_file() else {}
        reason = "timed out" if timed_out else detail.get("reason", "")
        attempts.append({"attempt": attempt, "outcome": outcome, "exit_code": process.returncode,
                         "duration_seconds": round(elapsed, 2), "reason": reason[:2000],
                         "log": str(log_path.relative_to(results))})
        if outcome != "infrastructure_error":
            break

    final = attempts[-1]["outcome"]
    binding_problem = ""
    # A harness gate (tests/gates/*.py built on gatelib) always writes a detail
    # record with its checks. Exit 0 without one means the script ran nothing --
    # a lost entry point, say -- and nothing is not a pass.
    if final == "pass" and "tests/gates/" in gate["command"] and ".py" in gate["command"]:
        if not detail or not detail.get("checks"):
            binding_problem = "the gate exited 0 without recording any check"
            final = "invalid_measurement"
    observed = detail.get("context", {}).get("artifact") if detail else None
    if gate.get("binds_artifact") and final == "pass" and not binding_problem:
        expected = candidate.get("binaries", {})
        # A gate reports the digests of the binaries it actually executed; the
        # server's is always required, the CLI's whenever the gate used it.
        if not observed or not observed.get("server_sha256"):
            binding_problem = "the gate did not report which binaries it ran"
        elif (observed.get("server_sha256") != expected.get("record-store-server")
              or ("cli_sha256" in observed and observed["cli_sha256"] != expected.get("record-store"))):
            binding_problem = "the gate ran binaries whose digests are not the candidate's"
        if binding_problem:
            final = "invalid_measurement"

    record = {
        "schema": 1,
        "gate": gate["id"],
        "matrix_version": matrix["matrix_version"],
        "scope": matrix["scope"],
        "definition_digest": definition_digest(gate),
        "stage": arguments.stage,
        "profile": profile,
        "seed": str(arguments.seed),
        "status": final,
        "reason": binding_problem or attempts[-1]["reason"],
        "commit": git("rev-parse", "HEAD"),
        "tree_clean": git("status", "--porcelain", "--untracked-files=no") == "",
        "candidate": {k: candidate.get(k) for k in ("commit", "binaries", "version", "source", "platform")} if candidate else None,
        "observed_artifact": observed,
        "attempts": attempts,
        "retried": len(attempts) > 1,
        "started_at": started_at,
        "finished_at": utc_now(),
        "environment": detail.get("environment") or {"os": platform.system(), "machine": platform.machine()},
        "checks": {"total": len(detail.get("checks", [])),
                   "failed": [c["name"] for c in detail.get("checks", []) if not c["passed"]]},
        "metrics": detail.get("metrics", {}),
        "notes": detail.get("notes", []),
        "run_url": (f"{os.environ['GITHUB_SERVER_URL']}/{os.environ['GITHUB_REPOSITORY']}/actions/runs/{os.environ['GITHUB_RUN_ID']}"
                    if os.environ.get("GITHUB_RUN_ID") else None),
    }
    results.mkdir(parents=True, exist_ok=True)
    (results / f"{gate['id']}.json").write_text(json.dumps(record, indent=2, default=str))
    print(f"--- {gate['id']}: {final.upper()}" + (f" ({record['reason']})" if record["reason"] else "")
          + (f" after {len(attempts)} attempts" if len(attempts) > 1 else ""), flush=True)
    return 0 if final == "pass" else 1


if __name__ == "__main__":
    sys.exit(main())
