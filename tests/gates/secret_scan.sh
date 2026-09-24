#!/usr/bin/env bash
# SEC-SECRETS: scans for committed secrets with a pinned, checksum-verified gitleaks.
#
#   tests/gates/secret_scan.sh tree      # the working tree (pull requests)
#   tests/gates/secret_scan.sh history   # every commit (release candidates)
#
# The binary is downloaded from the gitleaks release and refused unless its
# SHA-256 matches the value pinned below; bump both together. Findings are
# redacted in the report, which is written to GATE_EVIDENCE_DIR.
set -euo pipefail

mode="${1:-tree}"
version="8.30.1"
# Plain case rather than an associative array: macOS still ships bash 3.2.
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) platform=linux_x64 pinned=551f6fc83ea457d62a0d98237cbad105af8d557003051f41f3e7ca7b3f2470eb ;;
  Linux-aarch64 | Linux-arm64) platform=linux_arm64 pinned=e4a487ee7ccd7d3a7f7ec08657610aa3606637dab924210b3aee62570fb4b080 ;;
  Darwin-arm64) platform=darwin_arm64 pinned=b40ab0ae55c505963e365f271a8d3846efbc170aa17f2607f13df610a9aeb6a5 ;;
  *) echo "no pinned gitleaks for $(uname -s)-$(uname -m)" >&2; exit 75 ;;
esac

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cache="${GITLEAKS_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/record-store-gates/gitleaks-${version}}"
binary="$cache/gitleaks"
if [[ ! -x "$binary" ]]; then
  mkdir -p "$cache"
  archive="$cache/gitleaks.tar.gz"
  if ! curl --fail --silent --show-error --location --retry 3 --output "$archive" \
    "https://github.com/gitleaks/gitleaks/releases/download/v${version}/gitleaks_${version}_${platform}.tar.gz"; then
    exit 75
  fi
  actual="$( (command -v sha256sum >/dev/null && sha256sum "$archive" || shasum -a 256 "$archive") | cut -d' ' -f1)"
  if [[ "$actual" != "$pinned" ]]; then
    echo "gitleaks archive checksum mismatch: $actual" >&2
    rm -f "$archive"
    exit 1
  fi
  tar -xzf "$archive" -C "$cache" gitleaks
  rm -f "$archive"
fi

report="${GATE_EVIDENCE_DIR:-$(mktemp -d)}/gitleaks-report.json"
rm -f "$report"
common=(--config "$repository_root/.gitleaks.toml" --redact --no-banner --report-format json --report-path "$report" --exit-code 1)
case "$mode" in
  # What could be committed: tracked files plus untracked files that are not
  # ignored. A developer's ignored .env or data/ is not in scope -- it cannot
  # reach the repository -- and scanning it would bury real findings.
  tree)
    staging="$(mktemp -d)"
    trap 'rm -rf "$staging"' EXIT
    git -C "$repository_root" ls-files -z --cached --others --exclude-standard |
      python3 -c '
import os, shutil, sys
root, target = sys.argv[1], sys.argv[2]
for name in sys.stdin.buffer.read().split(b"\0"):
    if name:
        source = os.path.join(root, os.fsdecode(name))
        if os.path.isfile(source) and not os.path.islink(source):
            destination = os.path.join(target, os.fsdecode(name))
            os.makedirs(os.path.dirname(destination), exist_ok=True)
            shutil.copy2(source, destination)
' "$repository_root" "$staging"
    "$binary" dir "${common[@]}" "$staging" ;;
  history) "$binary" git "${common[@]}" "$repository_root" ;;
  *) echo "mode must be tree or history" >&2; exit 2 ;;
esac
