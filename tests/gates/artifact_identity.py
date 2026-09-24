#!/usr/bin/env python3
"""ART-IDENTITY: the binaries under test are the candidate, and say so.

Checks candidate.json (written by build-candidate.sh) against the files it
describes and against the repository:

  * both binaries exist and their SHA-256 digests are the recorded ones;
  * the candidate was built from this commit, from a clean tree, with this
    Cargo.lock;
  * both binaries report the workspace version -- and, on a tag, the tag's
    version (--expect-version).

What this cannot establish: the binaries embed no commit, and unreleased
builds report the previous release's version until the release bump, so for
anything but a tag the digests recorded at build time are the identity
(release/findings/RSG-006).
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from gatelib import REPOSITORY_ROOT, Gate, sha256_file, workspace_version  # noqa: E402


def main(gate: Gate) -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--expect-version")
    arguments, _ = parser.parse_known_args()
    manifest = json.loads((arguments.candidate / "candidate.json").read_text())
    gate.context["candidate"] = manifest
    for name, digest in manifest["binaries"].items():
        path = arguments.candidate / "bin" / name
        gate.require(f"{name} exists", path.is_file(), str(path))
        gate.check(f"{name} has the recorded digest", sha256_file(path) == digest)
        reported = subprocess.run([str(path), "--version"], capture_output=True, text=True).stdout.strip()
        gate.check(f"{name} reports the workspace version", reported.endswith(" " + workspace_version()), reported)
        if arguments.expect_version:
            gate.check(f"{name} reports the release version {arguments.expect_version}",
                       reported.endswith(" " + arguments.expect_version), reported)
    head = subprocess.run(["git", "-C", str(REPOSITORY_ROOT), "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
    gate.check("the candidate was built from this commit", manifest["commit"] == head, {"candidate": manifest["commit"], "head": head})
    gate.check("the candidate was built from a clean tree", manifest["tree_clean"] is True)
    gate.check("the candidate was built from this Cargo.lock",
               manifest["cargo_lock_sha256"] == sha256_file(REPOSITORY_ROOT / "Cargo.lock"))
    gate.context["artifact"] = {"server_sha256": manifest["binaries"]["record-store-server"],
                                "cli_sha256": manifest["binaries"]["record-store"],
                                "reported_version": manifest["version"]}


if __name__ == "__main__":
    Gate("ART-IDENTITY", "the tested binaries are the candidate").run(main)
