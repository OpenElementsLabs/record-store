#!/usr/bin/env python3
"""Turns gate results into a release decision.

    python3 tests/gates/evaluate.py --stage candidate --candidate out/candidate \\
        --results out/results [--results more/results ...] --out out/report

Reads the matrix, every result record it is given, the exception records in
release/exceptions/ and the quarantine in release/quarantine.toml, and writes
report.json (machine-readable) and report.md (the human decision).

The rules are written so that the summary cannot be greener than the evidence:

  * Every gate in the stage needs a result. A gate with no result -- because its
    job failed, was skipped, was cancelled, or never ran -- is MISSING, and a
    missing blocking gate blocks. A "skipped" outcome is not a pass.
  * A result counts only if it was produced by the current definition of the
    gate (definition digest), for this commit -- or, where the gate allows
    reuse, for an earlier commit no later than max_age_days old whose diff to
    this commit touches none of the gate's invalidating paths.
  * An artifact-bound result counts only if it names this candidate's binary
    digests.
  * Infrastructure errors and invalid measurements block like failures: they
    are the absence of evidence, not evidence of safety.
  * A blocking non-pass is waived only by a complete, unexpired exception record
    naming that gate. The evaluator never creates, extends or approves one.
  * An expired or incomplete quarantine entry blocks.
  * Open findings marked "Blocks release" block the candidate stage (and any
    stage run with --enforce-findings). In pr and integration a failing gate is
    downgraded to known_failure only when every failed check matches a pattern
    its open finding declares, so pull requests that fix defects can still land
    while any new failure blocks.
  * Retried results are listed even when they passed.

Exit status: 0 when the decision is READY, READY WITH EXCEPTIONS or (with
--defer) READY SO FAR; 1 when it is BLOCKED; 2 when the matrix or an exception
record is itself malformed. READY SO FAR is never a release decision: the
workflow that deferred a gate must evaluate again once it has run.
"""

from __future__ import annotations

import argparse
import datetime
import fnmatch
import json
import subprocess
import sys
import tomllib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from record import definition_digest  # noqa: E402

REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
REQUIRED_GATE_FIELDS = (
    "id", "title", "area", "guarantee", "modes", "platforms", "stages", "blocking", "command",
    "environment", "fixtures", "failure_injection", "evidence", "timeout_minutes", "limitations",
)
REQUIRED_EXCEPTION_FIELDS = ("id", "gate", "guarantee", "evidence", "risk", "owner", "approved_by", "created", "expires")
REQUIRED_QUARANTINE_FIELDS = ("test", "owner", "reason", "expires", "coverage_risk", "issue")
STATUS_ORDER = ["fail", "infrastructure_error", "invalid_measurement", "missing", "stale", "skipped", "known_failure",
                "deferred", "excepted", "pass"]


def today() -> datetime.date:
    return datetime.datetime.now(datetime.timezone.utc).date()


def parse_date(value) -> datetime.date:
    if isinstance(value, datetime.date):
        return value
    return datetime.date.fromisoformat(str(value))


def validate_matrix(matrix: dict, path: Path) -> list[str]:
    problems = []
    seen = set()
    for gate in matrix.get("gate", []):
        for field in REQUIRED_GATE_FIELDS:
            if field not in gate or gate[field] in ("", None):
                problems.append(f"{path}: gate {gate.get('id', '?')} lacks {field}")
        if gate.get("id") in seen:
            problems.append(f"{path}: duplicate gate id {gate['id']}")
        seen.add(gate.get("id"))
        for stage in gate.get("stages", []):
            if stage not in matrix.get("stages", {}):
                problems.append(f"{path}: gate {gate.get('id')} names unknown stage {stage}")
    return problems


def load_results(directories: list[Path]) -> dict[str, list[dict]]:
    found: dict[str, list[dict]] = {}
    for directory in directories:
        for path in sorted(directory.rglob("*.json")):
            if "evidence" in path.relative_to(directory).parts:
                continue
            try:
                record = json.loads(path.read_text())
            except (json.JSONDecodeError, UnicodeDecodeError):
                continue
            if isinstance(record, dict) and record.get("schema") == 1 and "gate" in record and "status" in record:
                record["_path"] = str(path)
                found.setdefault(record["gate"], []).append(record)
    return found


def changed_paths(since: str, until: str) -> list[str] | None:
    result = subprocess.run(["git", "-C", str(REPOSITORY_ROOT), "diff", "--name-only", f"{since}..{until}"],
                            capture_output=True, text=True)
    return result.stdout.split() if result.returncode == 0 else None


def reusable(gate: dict, record: dict, commit: str) -> tuple[bool, str]:
    reuse = gate.get("reuse", {})
    max_age = int(reuse.get("max_age_days", 0))
    if max_age <= 0:
        return False, "this gate's results are never reused across commits"
    finished = datetime.datetime.strptime(record["finished_at"], "%Y-%m-%dT%H:%M:%SZ").date()
    if (today() - finished).days > max_age:
        return False, f"result is older than {max_age} days"
    changed = changed_paths(record["commit"], commit)
    if changed is None:
        return False, f"cannot diff {record['commit'][:12]}..{commit[:12]} (history unavailable)"
    hits = [path for path in changed if any(fnmatch.fnmatch(path, pattern) for pattern in reuse.get("invalidated_by", ["**"]))]
    if hits:
        return False, f"invalidated by changes to {', '.join(hits[:5])}"
    return True, f"reused from {record['commit'][:12]} ({(today() - finished).days} d old, no invalidating change)"


PROFILE_OF_STAGE = {"pr": "pr", "integration": "pr", "scheduled": "scheduled", "candidate": "candidate"}
PROFILE_STRENGTH = {"pr": 0, "scheduled": 1, "candidate": 2, "endurance": 2}


def select_result(gate: dict, records: list[dict], commit: str, candidate: dict | None,
                  stage: str = "candidate") -> tuple[dict | None, str, str]:
    """Returns (record, status, explanation)."""
    if not records:
        return None, "missing", "no result was produced"
    digest = definition_digest(gate)
    explanations = []
    ordered = sorted(records, key=lambda r: r.get("finished_at", ""), reverse=True)
    for record in ordered:
        if record.get("definition_digest") != digest:
            explanations.append(f"{Path(record['_path']).name}: produced by a different gate definition")
            continue
        # A gate whose workload depends on the profile counts only when it ran
        # at least as heavy a profile as this stage requires: two crashes per
        # mode are not evidence for a stage that requires five.
        if "${PROFILE}" in gate["command"]:
            required = PROFILE_STRENGTH[PROFILE_OF_STAGE[stage]]
            if PROFILE_STRENGTH.get(record.get("profile", "pr"), 0) < required:
                explanations.append(f"{Path(record['_path']).name}: ran the lighter {record.get('profile')!r} profile")
                continue
        if gate.get("binds_artifact") and candidate:
            expected = candidate.get("binaries")
            if (record.get("candidate") or {}).get("binaries") != expected:
                explanations.append(f"{Path(record['_path']).name}: bound to other binaries")
                continue
        if record.get("commit") == commit:
            return record, record["status"], "same commit"
        ok, why = reusable(gate, record, commit)
        if ok:
            return record, record["status"], why
        explanations.append(f"{Path(record['_path']).name}: {why}")
    return None, "stale", "; ".join(explanations) or "no usable result"


def load_exceptions(directory: Path) -> tuple[list[dict], list[str]]:
    records, problems = [], []
    if not directory.is_dir():
        return records, problems
    for path in sorted(directory.glob("*.toml")):
        with open(path, "rb") as handle:
            record = tomllib.load(handle)
        missing = [field for field in REQUIRED_EXCEPTION_FIELDS if not str(record.get(field, "")).strip()]
        if missing:
            problems.append(f"{path.name}: incomplete exception record, missing {', '.join(missing)}")
            continue
        record["_path"] = str(path)
        records.append(record)
    return records, problems


def read_findings(directory: Path | None) -> list[dict]:
    """Every finding's header table: id, status, blocks release, gate, known failing checks."""
    findings = []
    if not directory or not directory.is_dir():
        return findings
    for path in sorted(directory.glob("RSG-*.md")):
        fields = {}
        for line in path.read_text().splitlines():
            cells = [cell.strip() for cell in line.strip().strip("|").split("|")]
            if len(cells) == 2:
                fields[cells[0].lower()] = cells[1]
        identifier = "-".join(path.name.split("-")[:2])
        findings.append({
            "id": identifier,
            "open": fields.get("status", "").lower().startswith("open"),
            "blocks_release": fields.get("blocks release", "").lower().startswith("yes"),
            "gate": fields.get("gate", "").split(" ")[0],
            "known_checks": [part.strip().strip("`") for part in fields.get("known failing checks", "").split(";") if part.strip()],
        })
    return findings


def open_blocking_findings(directory: Path) -> list[str]:
    """Ids of findings whose header table says Status: open and Blocks release: yes."""
    return [f["id"] for f in read_findings(directory) if f["open"] and f["blocks_release"]]


def known_failure(row: dict, findings: list[dict]) -> str | None:
    """The finding that explains every failed check of this gate, if one does.

    Only a failure fully explained by an open, recorded finding qualifies: one new
    failed check in the same gate and it is an ordinary failure again.
    """
    failed = row["failed_checks"]
    if row["status"] != "fail" or not failed:
        return None
    for finding in findings:
        if finding["open"] and finding["gate"] == row["gate"] and finding["known_checks"]:
            if all(any(pattern in check for pattern in finding["known_checks"]) for check in failed):
                return finding["id"]
    return None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--matrix", type=Path, default=REPOSITORY_ROOT / "release/gates.toml")
    parser.add_argument("--stage", required=True, choices=["pr", "integration", "scheduled", "candidate"])
    parser.add_argument("--candidate", type=Path)
    parser.add_argument("--results", type=Path, action="append", default=[])
    parser.add_argument("--exceptions", type=Path, default=REPOSITORY_ROOT / "release/exceptions")
    parser.add_argument("--quarantine", type=Path, default=REPOSITORY_ROOT / "release/quarantine.toml")
    parser.add_argument("--findings", type=Path, help="defaults to release/findings for the standalone scope")
    parser.add_argument("--enforce-findings", action="store_true",
                        help="apply release rules outside the candidate stage (release.yml's pre-publish run)")
    parser.add_argument("--defer", action="append", default=[],
                        help="a gate that cannot have run yet at this point (e.g. ART-PACKAGE before publishing); "
                             "reported as deferred, never as passed")
    parser.add_argument("--commit", help="defaults to HEAD")
    parser.add_argument("--out", type=Path, required=True)
    arguments = parser.parse_args()

    with open(arguments.matrix, "rb") as handle:
        matrix = tomllib.load(handle)
    problems = validate_matrix(matrix, arguments.matrix)
    exceptions, exception_problems = load_exceptions(arguments.exceptions)
    problems += exception_problems
    if problems:
        for problem in problems:
            print(f"error: {problem}", file=sys.stderr)
        return 2

    commit = arguments.commit or subprocess.run(["git", "-C", str(REPOSITORY_ROOT), "rev-parse", "HEAD"],
                                                capture_output=True, text=True).stdout.strip()
    candidate = None
    if arguments.candidate and (arguments.candidate / "candidate.json").is_file():
        candidate = json.loads((arguments.candidate / "candidate.json").read_text())
    results = load_results(arguments.results)
    findings_directory = arguments.findings or (REPOSITORY_ROOT / "release/findings" if matrix["scope"] == "standalone" else None)
    findings = read_findings(findings_directory)

    rows = []
    for gate in matrix["gate"]:
        if arguments.stage not in gate["stages"]:
            continue
        record, status, explanation = select_result(gate, results.get(gate["id"], []), commit, candidate,
                                                    arguments.stage)
        if gate["id"] in arguments.defer and status in ("missing", "stale"):
            status, explanation = "deferred", "evaluated later in this workflow; not yet evidence"
        row = {
            "gate": gate["id"], "title": gate["title"], "area": gate["area"], "blocking": gate["blocking"],
            "status": status, "explanation": explanation, "reason": (record or {}).get("reason", ""),
            "retried": bool((record or {}).get("retried")),
            "attempts": len((record or {}).get("attempts", [])),
            "notes": (record or {}).get("notes", []),
            "failed_checks": (record or {}).get("checks", {}).get("failed", []),
            "result_commit": (record or {}).get("commit"), "run_url": (record or {}).get("run_url"),
            "limitations": gate["limitations"], "exception": None,
        }
        release_rules = arguments.stage == "candidate" or arguments.enforce_findings
        if not release_rules and gate["blocking"]:
            explained = known_failure(row, findings)
            if explained:
                row["status"] = "known_failure"
                row["explanation"] = f"every failed check is tracked by open finding {explained}; blocks release, not this stage"
        if row["status"] not in ("pass", "known_failure") and gate["blocking"]:
            for exception in exceptions:
                if exception["gate"] != gate["id"]:
                    continue
                if parse_date(exception["expires"]) < today():
                    row["notes"].append(f"exception {exception['id']} expired on {exception['expires']}")
                    continue
                scope = exception.get("candidate_commit")
                if scope and scope != commit:
                    row["notes"].append(f"exception {exception['id']} is scoped to {scope[:12]}")
                    continue
                row["exception"] = {k: exception[k] for k in REQUIRED_EXCEPTION_FIELDS}
                row["status_before_exception"] = status
                row["status"] = "excepted"
                break
        rows.append(row)

    # Quarantine hygiene is part of every stage: an expired entry is a
    # coverage gap that nobody is watching any more.
    quarantine_problems = []
    if arguments.quarantine.is_file():
        with open(arguments.quarantine, "rb") as handle:
            quarantine = tomllib.load(handle)
        for entry in quarantine.get("entry", []):
            missing = [field for field in REQUIRED_QUARANTINE_FIELDS if not str(entry.get(field, "")).strip()]
            if missing:
                quarantine_problems.append(f"{entry.get('test', '?')}: missing {', '.join(missing)}")
            elif parse_date(entry["expires"]) < today():
                quarantine_problems.append(f"{entry['test']}: quarantine expired on {entry['expires']}")
    rows.append({
        "gate": "QUARANTINE", "title": "Every quarantined test has an owner, reason, expiry and risk; none expired",
        "area": "correctness", "blocking": True, "status": "fail" if quarantine_problems else "pass",
        "explanation": "; ".join(quarantine_problems) or "quarantine is complete and current", "reason": "",
        "retried": False, "attempts": 0, "notes": [], "failed_checks": quarantine_problems, "result_commit": commit,
        "run_url": None, "limitations": "", "exception": None,
    })

    # Open, release-blocking findings block until fixed or excepted, so a defect
    # recorded in prose cannot be forgotten by the machinery.
    # Open, release-blocking findings block a release -- the candidate stage and
    # release.yml's pre-publish run -- but not ordinary pull requests, which
    # must stay mergeable so that the fixes themselves can land.
    release_rules = arguments.stage == "candidate" or arguments.enforce_findings
    open_findings = [f["id"] for f in findings if f["open"] and f["blocks_release"]] if release_rules else []
    waived = []
    for finding in list(open_findings):
        for exception in exceptions:
            if exception["gate"] == finding and parse_date(exception["expires"]) >= today():
                waived.append(f"{finding} (exception {exception['id']})")
                open_findings.remove(finding)
                break
    rows.append({
        "gate": "FINDINGS", "title": "No open finding that blocks release", "area": "correctness", "blocking": True,
        "status": "fail" if open_findings else ("excepted" if waived else "pass"),
        "explanation": ("open: " + ", ".join(open_findings)) if open_findings else
                       (("excepted: " + ", ".join(waived)) if waived else "none open"),
        "reason": "", "retried": False, "attempts": 0, "notes": [], "failed_checks": open_findings,
        "result_commit": commit, "run_url": None, "limitations": "", "exception": None,
    })

    blockers = [row for row in rows if row["blocking"] and row["status"] not in ("pass", "excepted", "deferred", "known_failure")]
    deferred = [row["gate"] for row in rows if row["status"] == "deferred"]
    excepted = [row for row in rows if row["status"] == "excepted"]
    advisory = [row for row in rows if not row["blocking"] and row["status"] != "pass"]
    decision = "BLOCKED" if blockers else ("READY WITH EXCEPTIONS" if excepted else "READY")
    if deferred and not blockers:
        decision = "READY SO FAR"
    report = {
        "schema": 1, "scope": matrix["scope"], "matrix_version": matrix["matrix_version"],
        "stage": arguments.stage, "commit": commit,
        "candidate": {k: (candidate or {}).get(k) for k in ("commit", "version", "binaries", "source", "platform", "cargo_lock_sha256")},
        "decision": decision, "evaluated_at": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "blockers": [row["gate"] for row in blockers], "deferred": deferred, "excepted": [row["gate"] for row in excepted],
        "advisory_not_passing": [row["gate"] for row in advisory],
        "known_failures": [row["gate"] for row in rows if row["status"] == "known_failure"],
        "retried": [row["gate"] for row in rows if row["retried"]],
        "gates": rows,
    }
    if candidate and candidate.get("commit") != commit:
        report["decision"] = decision = "BLOCKED"
        report["blockers"].append("CANDIDATE-COMMIT")
        report["candidate_mismatch"] = f"candidate.json was built from {candidate.get('commit')}, evaluating {commit}"

    arguments.out.mkdir(parents=True, exist_ok=True)
    (arguments.out / "report.json").write_text(json.dumps(report, indent=2, default=str))
    (arguments.out / "report.md").write_text(render(report))
    print(render(report))
    return 0 if decision.startswith("READY") else 1


def render(report: dict) -> str:
    symbol = {"pass": "pass", "fail": "**FAIL**", "infrastructure_error": "**INFRA**",
              "invalid_measurement": "**INVALID**", "missing": "**MISSING**", "stale": "**STALE**",
              "skipped": "**SKIPPED**", "excepted": "excepted", "deferred": "deferred",
              "known_failure": "known failure"}
    candidate = report["candidate"]
    lines = [
        f"# {report['scope'].capitalize()} release gates: {report['decision']}",
        "",
        f"- Stage: `{report['stage']}` · matrix `{report['matrix_version']}`",
        f"- Commit: `{report['commit']}`",
    ]
    if candidate.get("binaries"):
        lines.append(f"- Candidate: {candidate.get('version')} ({candidate.get('source')}, {candidate.get('platform')}); "
                     + ", ".join(f"`{name}` sha256 `{digest[:16]}…`" for name, digest in candidate["binaries"].items()))
    if report.get("candidate_mismatch"):
        lines.append(f"- **{report['candidate_mismatch']}**")
    lines.append("")
    if report["blockers"]:
        lines.append(f"**Blocking:** {', '.join(report['blockers'])}")
    if report.get("known_failures"):
        lines.append(f"**Known failures (tracked findings; block release):** {', '.join(report['known_failures'])}")
    if report.get("deferred"):
        lines.append(f"**Deferred (not yet evidence):** {', '.join(report['deferred'])}")
    if report["excepted"]:
        lines.append(f"**Released under exception:** {', '.join(report['excepted'])}")
    if report["advisory_not_passing"]:
        lines.append(f"**Advisory, not passing:** {', '.join(report['advisory_not_passing'])}")
    if report["retried"]:
        lines.append(f"**Retried (infrastructure):** {', '.join(report['retried'])}")
    lines += ["", "| Gate | Area | Blocking | Result | Evidence |", "| --- | --- | --- | --- | --- |"]
    for row in sorted(report["gates"], key=lambda r: (STATUS_ORDER.index(r["status"]) if r["status"] in STATUS_ORDER else 0, r["gate"])):
        detail = row["reason"] or row["explanation"]
        if row["failed_checks"]:
            detail = "; ".join(row["failed_checks"][:3]) + (" …" if len(row["failed_checks"]) > 3 else "")
        if row["exception"]:
            detail = f"exception {row['exception']['id']} (owner {row['exception']['owner']}, expires {row['exception']['expires']}): {detail}"
        detail = detail.replace("|", "/").replace("\n", " ")[:300]
        lines.append(f"| {row['gate']} | {row['area']} | {'yes' if row['blocking'] else 'no'} | "
                     f"{symbol.get(row['status'], row['status'])} | {detail} |")
    notes = [(row["gate"], note) for row in report["gates"] for note in row["notes"]]
    if notes:
        lines += ["", "## Notes", ""] + [f"- {gate}: {note}" for gate, note in notes]
    return "\n".join(lines) + "\n"


if __name__ == "__main__":
    sys.exit(main())
