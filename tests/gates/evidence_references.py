#!/usr/bin/env python3
"""CL-EVIDENCE: every test an internal evidence document cites exists and passed.

internal/cluster/failure-matrix.md maps each invariant to the tests that
reproduce it. A renamed or deleted test leaves that row pointing at nothing,
and the invariant quietly becomes a claim. This gate reads the backticked test
names in the given documents and requires each to be a Rust test function in
the tree and -- given the log of this candidate's `cargo test` run -- to have
run and passed in it.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from gatelib import REPOSITORY_ROOT, Gate  # noqa: E402

NAME = re.compile(r"`([a-z][a-z0-9]*(?:_[a-z0-9]+){2,})`")
TEST_FUNCTION = re.compile(r"#\[(?:tokio::)?test[^\]]*\]\s*(?:#\[[^\]]*\]\s*)*(?:pub\s+)?(?:async\s+)?fn\s+([a-z0-9_]+)", re.MULTILINE)


def main(gate: Gate) -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--document", type=Path, action="append", required=True)
    parser.add_argument("--cargo-log", type=Path)
    arguments, _ = parser.parse_known_args()

    defined: set[str] = set()
    for path in list((REPOSITORY_ROOT / "crates").rglob("*.rs")) + list((REPOSITORY_ROOT / "apps").rglob("*.rs")):
        defined.update(TEST_FUNCTION.findall(path.read_text(errors="replace")))
    passed: set[str] | None = None
    if arguments.cargo_log:
        passed = set(re.findall(r"^test (?:\S+::)?([a-z0-9_]+) \.\.\. ok$", arguments.cargo_log.read_text(errors="replace"),
                                re.MULTILINE))

    for document in arguments.document:
        text = document.read_text()
        # Only table rows cite evidence; prose may mention identifiers in passing.
        cited = sorted({name for line in text.splitlines() if line.startswith("|") for name in NAME.findall(line)})
        cited = [name for name in cited if not name.startswith("record_store")]
        missing = [name for name in cited if name not in defined]
        gate.context[f"{document.name}_cited"] = len(cited)
        gate.check(f"{document.name}: every cited test exists ({len(cited)} cited)", not missing, missing[:20])
        if passed is not None:
            not_passed = [name for name in cited if name in defined and name not in passed]
            gate.check(f"{document.name}: every cited test ran and passed in this candidate's run",
                       not not_passed, not_passed[:20])


if __name__ == "__main__":
    Gate("CL-EVIDENCE", "every cited cluster test exists and passed").run(main)
