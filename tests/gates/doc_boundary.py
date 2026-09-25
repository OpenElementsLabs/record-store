#!/usr/bin/env python3
"""DOC-BOUNDARY: the public documentation stays standalone.

Clustering is engineering-internal. Three things keep it that way:

  * mkdocs.yml must build from docs/ and nothing under internal/ may sit inside
    docs_dir, so the documentation build cannot pick it up;
  * if a built site is supplied, no page in it may carry internal material (every
    internal file starts with "Internal. Not published.");
  * every line in docs/ or mkdocs.yml that mentions clustering, replication,
    consensus or erasure coding must be listed verbatim in the allowlist. The
    allowlisted lines are the ones that say these do *not* exist. A new mention
    fails until someone reviews it and either removes it or adds it here.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from gatelib import REPOSITORY_ROOT, Gate  # noqa: E402

TERMS = re.compile(r"cluster|replicat|consensus|raft\b|erasure|quorum|multi-node|rs cluster|record-store cluster|join[- ]token",
                   re.IGNORECASE)
INTERNAL_MARKER = "Internal. Not published."


def main(gate: Gate) -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--allowlist", type=Path, default=REPOSITORY_ROOT / "release/public-docs-allowlist.txt")
    parser.add_argument("--site", type=Path)
    arguments, _ = parser.parse_known_args()

    mkdocs = (REPOSITORY_ROOT / "mkdocs.yml").read_text()
    docs_dir = re.search(r"^docs_dir:\s*(\S+)", mkdocs, re.MULTILINE)
    docs_dir = (docs_dir.group(1) if docs_dir else "docs").strip("'\"")
    gate.check("the documentation build reads docs/", docs_dir in ("docs", "docs/"), docs_dir)
    internal_inside = [str(p) for p in (REPOSITORY_ROOT / docs_dir).rglob("*") if INTERNAL_MARKER in _text(p)]
    gate.check("no internal material is inside docs_dir", not internal_inside, internal_inside[:10])

    allowed = set()
    for line in arguments.allowlist.read_text().splitlines():
        if line.strip() and not line.startswith("#"):
            allowed.add(line.rstrip("\n"))
    mentions = []
    for path in sorted([REPOSITORY_ROOT / "mkdocs.yml", *(REPOSITORY_ROOT / docs_dir).rglob("*.md")]):
        relative = str(path.relative_to(REPOSITORY_ROOT))
        for line in path.read_text(errors="replace").splitlines():
            if TERMS.search(line):
                mentions.append(f"{relative}: {line.strip()}")
    unreviewed = [m for m in mentions if m not in allowed]
    gate.context["mentions"] = mentions
    gate.check("every public mention of clustering-related terms is a reviewed line", not unreviewed, unreviewed[:20])
    stale = sorted(allowed - set(mentions))
    if stale:
        gate.note(f"allowlisted lines no longer present (remove them): {len(stale)}")

    if arguments.site:
        leaked = [str(p.relative_to(arguments.site)) for p in arguments.site.rglob("*.html") if INTERNAL_MARKER in _text(p)]
        gate.check("the built site contains no internal page", not leaked, leaked[:10])
        gate.check("the built site has no internal/ directory", not (arguments.site / "internal").exists())


def _text(path: Path) -> str:
    try:
        return path.read_text(errors="replace") if path.is_file() else ""
    except OSError:
        return ""


if __name__ == "__main__":
    Gate("DOC-BOUNDARY", "public documentation stays standalone").run(main)
