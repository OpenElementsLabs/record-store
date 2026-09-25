#!/usr/bin/env python3
"""COR-SKIPS: nothing mandatory is skipped, ignored, or focused.

A skipped test reports as green in most runners. This gate makes every skip a
visible, owned, expiring decision:

  * every skip marker in the tree must match an entry in release/quarantine.toml
    carrying an owner, reason, expiry, coverage risk and issue;
  * a focus marker (`.only`) always fails: it silently disables every other test
    in its file;
  * given the log of the real `cargo test` run, the number of ignored and
    filtered-out tests it reports must not exceed what is quarantined -- the
    runtime cross-check for a skip the patterns below do not recognise;
  * an expired quarantine entry fails (the evaluator checks this too).
"""

from __future__ import annotations

import argparse
import datetime
import re
import sys
import tomllib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from gatelib import REPOSITORY_ROOT, Gate  # noqa: E402

EXCLUDED = {"target", "node_modules", ".git", "site", ".next", "playwright-report", "test-results", "dist"}
SKIP_PATTERNS = {
    ".rs": [re.compile(r"#\[\s*ignore\b")],
    ".ts": [re.compile(r"\b(?:test|it|describe)\.(?:skip|fixme)\s*\("), re.compile(r"\b(?:xit|xdescribe)\s*\(")],
    ".tsx": [re.compile(r"\b(?:test|it|describe)\.(?:skip|fixme)\s*\(")],
    ".js": [re.compile(r"\b(?:test|it|describe)\.(?:skip|fixme)\s*\(")],
    ".mjs": [re.compile(r"\b(?:test|it|describe)\.(?:skip|fixme)\s*\(")],
    ".py": [re.compile(r"\bpytest\.skip\b|@pytest\.mark\.skip|@unittest\.skip")],
    ".go": [re.compile(r"\bt\.Skip(?:f|Now)?\s*\(")],
    ".java": [re.compile(r"@Disabled\b|@Ignore\b")],
}
FOCUS_PATTERNS = [re.compile(r"\b(?:test|it|describe)\.only\s*\("), re.compile(r"\bfdescribe\s*\(|\bfit\s*\(")]
CARGO_RESULT = re.compile(r"test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out")


def source_files() -> list[Path]:
    files = []
    for path in REPOSITORY_ROOT.rglob("*"):
        if path.is_file() and path.suffix in SKIP_PATTERNS and not (set(path.relative_to(REPOSITORY_ROOT).parts) & EXCLUDED):
            files.append(path)
    return sorted(files)


def identify(lines: list[str], index: int, path: Path) -> str:
    """Names the skipped test: the next Rust fn, or the title a JS/Go test gives."""
    for line in lines[index : index + 8]:
        match = re.search(r"\bfn\s+([A-Za-z0-9_]+)", line) or re.search(r"""\(\s*['"`]([^'"`]+)['"`]""", line)
        if match:
            return match.group(1)
    return lines[index].strip()


def main(gate: Gate) -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--quarantine", type=Path, default=REPOSITORY_ROOT / "release/quarantine.toml")
    parser.add_argument("--cargo-log", type=Path)
    arguments, _ = parser.parse_known_args()
    with open(arguments.quarantine, "rb") as handle:
        entries = tomllib.load(handle).get("entry", [])
    quarantined = {entry["test"] for entry in entries}
    today = datetime.datetime.now(datetime.timezone.utc).date()

    # The gate's own patterns would match themselves; everything else is scanned.
    this_file = Path(__file__).resolve()
    found, focused = [], []
    for path in source_files():
        if path.resolve() == this_file:
            continue
        lines = path.read_text(errors="replace").splitlines()
        relative = str(path.relative_to(REPOSITORY_ROOT))
        for index, line in enumerate(lines):
            if any(pattern.search(line) for pattern in FOCUS_PATTERNS) and path.suffix in (".ts", ".tsx", ".js", ".mjs"):
                focused.append(f"{relative}:{index + 1}")
            if any(pattern.search(line) for pattern in SKIP_PATTERNS[path.suffix]):
                found.append(f"{relative}::{identify(lines, index, path)}")
    gate.context["skip_markers"] = found
    gate.context["quarantine_entries"] = sorted(quarantined)
    unlisted = [marker for marker in found if marker not in quarantined]
    gate.check("every skip marker in the tree is quarantined with an owner and expiry", not unlisted, unlisted[:20])
    gate.check("no test is focused with .only", not focused, focused[:20])
    expired = [e["test"] for e in entries if datetime.date.fromisoformat(str(e["expires"])) < today]
    gate.check("no quarantine entry has expired", not expired, expired)
    stale = sorted(quarantined - set(found))
    if stale:
        gate.note(f"quarantine entries whose marker is gone (remove them): {stale}")

    if arguments.cargo_log:
        if not arguments.cargo_log.is_file():
            gate.require("the cargo test log exists", False, str(arguments.cargo_log))
        totals = [0, 0, 0, 0, 0]
        text = arguments.cargo_log.read_text(errors="replace")
        for match in CARGO_RESULT.finditer(text):
            totals = [a + int(b) for a, b in zip(totals, match.groups())]
        passed, failed, ignored, _, filtered = totals
        gate.context["cargo_totals"] = {"passed": passed, "failed": failed, "ignored": ignored, "filtered_out": filtered}
        gate.metric("cargo_tests_passed", float(passed), "tests")
        gate.require("the cargo log contains test results", passed + failed > 0)
        rust_quarantined = sum(1 for entry in entries if entry["test"].split("::", 1)[0].endswith(".rs"))
        gate.check(f"cargo reports no more ignored tests ({ignored}) than are quarantined ({rust_quarantined})",
                   ignored <= rust_quarantined)
        gate.check("the full run filtered nothing out", filtered == 0, filtered)


if __name__ == "__main__":
    Gate("COR-SKIPS", "nothing mandatory is skipped, ignored, or focused").run(main)
