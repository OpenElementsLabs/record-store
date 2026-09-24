#!/usr/bin/env bash
# Runs every gate of one stage and evaluates the result.
#
#   tests/gates/run-stage.sh STAGE OUT_DIR [GATE_ID ...]
#
# STAGE is pr, integration, scheduled or candidate (see release/gates.toml).
# OUT_DIR receives candidate/ (the binaries and candidate.json), results/ (one
# record per gate plus evidence/), report/ (report.json, report.md).
#
# Naming gate ids runs only those; the evaluation still covers the whole stage,
# so anything not run shows as MISSING rather than disappearing.
#
# Environment:
#   CANDIDATE_BIN_DIR       adopt these binaries instead of building (release.yml
#                           passes the ones extracted from the published image)
#   PREVIOUS_BIN_DIR        0.1.3 binaries for CMP-UPGRADE; otherwise built once
#                           from tag v0.1.3
#   GATE_MATRIX             defaults to release/gates.toml
#   GATE_EVALUATE=0         run the gates but skip the evaluation (CI runs gates in
#                           several jobs and evaluates once, in its report job)
#   COR_UNIT_LOG            a cargo test log to cross-check (default: this run's)
#
# Gates that only exist inside release.yml (ART-PACKAGE) are not run here and
# are reported MISSING by a local candidate evaluation. That is the intended
# outcome: a laptop cannot prove what a published image contains.
set -uo pipefail

stage="${1:?usage: run-stage.sh STAGE OUT_DIR [GATE_ID ...]}"
out="${2:?usage: run-stage.sh STAGE OUT_DIR [GATE_ID ...]}"
shift 2
repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
matrix="${GATE_MATRIX:-$repository_root/release/gates.toml}"
mkdir -p "$out"
out="$(cd "$out" && pwd)"
results="$out/results"
mkdir -p "$results"

# Pinned harness dependencies in an isolated environment, first on PATH so the
# matrix's `python3` is this one.
if [[ ! -x "$out/venv/bin/python" ]]; then
  python3 -m venv "$out/venv" || exit 75
  "$out/venv/bin/pip" install --quiet --requirement "$repository_root/tests/gates/requirements.txt" || exit 75
fi
export PATH="$out/venv/bin:$PATH"

if [[ ! -f "$out/candidate/candidate.json" ]]; then
  "$repository_root/tests/gates/build-candidate.sh" "$out/candidate" || exit 1
fi

gates=("$@")
if [[ ${#gates[@]} -eq 0 ]]; then
  while IFS= read -r gate; do gates+=("$gate"); done < <(python3 - "$matrix" "$stage" <<'EOF'
import sys, tomllib
matrix = tomllib.load(open(sys.argv[1], "rb"))
for gate in matrix["gate"]:
    # ART-PACKAGE is produced by release.yml itself, never by this script.
    if sys.argv[2] in gate["stages"] and gate["id"] != "ART-PACKAGE":
        print(gate["id"])
EOF
  )
fi

needs_previous=false
for gate in "${gates[@]}"; do [[ "$gate" == "CMP-UPGRADE" ]] && needs_previous=true; done
previous_args=()
if $needs_previous; then
  previous="$out/previous"
  if [[ -n "${PREVIOUS_BIN_DIR:-}" ]]; then
    mkdir -p "$previous/bin" && cp "$PREVIOUS_BIN_DIR/record-store" "$PREVIOUS_BIN_DIR/record-store-server" "$previous/bin/"
    [[ -f "$PREVIOUS_BIN_DIR/../record-store.example.toml" ]] && cp "$PREVIOUS_BIN_DIR/../record-store.example.toml" "$previous/"
  elif [[ ! -x "$previous/bin/record-store-server" ]]; then
    # Built from the tag, in its own worktree and target directory, so nothing
    # of the candidate's build can leak into it.
    worktree="$out/previous-src"
    git -C "$repository_root" worktree add --detach "$worktree" v0.1.3 >/dev/null 2>&1 || true
    (cd "$worktree" && cargo build --release --locked --bin record-store-server --bin record-store) || exit 75
    mkdir -p "$previous/bin"
    cp "$worktree/target/release/record-store" "$worktree/target/release/record-store-server" "$previous/bin/"
    cp "$worktree/record-store.example.toml" "$previous/"
  fi
  if [[ ! -f "$previous/record-store.example.toml" ]]; then
    git -C "$repository_root" show v0.1.3:record-store.example.toml > "$previous/record-store.example.toml" 2>/dev/null \
      || rm -f "$previous/record-store.example.toml"
  fi
  previous_args=(--previous "$previous")
fi

failed=()
for gate in "${gates[@]}"; do
  case "$gate" in
    COR-SKIPS | CL-EVIDENCE)
      # Cross-checked against the real test run of this candidate.
      if [[ -z "${COR_UNIT_LOG:-}" ]]; then
        log="$results/evidence/COR-UNIT/attempt-1/output.log"
        [[ -f "$log" ]] || log="$results/evidence/CL-SUITES/attempt-1/output.log"
        [[ -f "$log" ]] && export COR_UNIT_LOG="$log"
      fi ;;
    DOC-BOUNDARY)
      if command -v mkdocs >/dev/null && mkdocs build --strict --quiet --config-file "$repository_root/mkdocs.yml" --site-dir "$out/site"; then
        export SITE_DIR="$out/site"
      fi ;;
    SEC-SECRETS)
      [[ "$stage" == "candidate" ]] && export SECRET_SCAN_MODE=history ;;
  esac
  python3 "$repository_root/tests/gates/record.py" --gate "$gate" --matrix "$matrix" --stage "$stage" \
    --candidate "$out/candidate" --results "$results" ${previous_args[@]+"${previous_args[@]}"} || failed+=("$gate")
done

if [[ ${#failed[@]} -gt 0 ]]; then
  echo "gates not passing: ${failed[*]}" >&2
fi
if [[ "${GATE_EVALUATE:-1}" == "0" ]]; then
  [[ ${#failed[@]} -eq 0 ]]
  exit $?
fi
python3 "$repository_root/tests/gates/evaluate.py" --matrix "$matrix" --stage "$stage" \
  --candidate "$out/candidate" --results "$results" --out "$out/report"
