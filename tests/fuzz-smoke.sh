#!/usr/bin/env bash
set -euo pipefail

# Fuzz targets rot quietly. They live in their own workspace, so nothing else in
# CI compiles them, and a parser that gets renamed or re-shaped leaves behind a
# target that no longer reaches it -- coverage on paper and nothing in practice.
#
# This builds every target and runs each one briefly. Twenty seconds is not a
# search; a real campaign runs for hours against a corpus that is kept between
# runs. What this length does buy is the guarantee that each harness still
# compiles, still reaches its parser, and still holds its assertions, so the
# pull request that breaks one fails rather than the next person to try fuzzing.
#
# FUZZ_SECONDS overrides the per-target budget for a longer local run.
# FUZZ_TOTAL_SECONDS instead sets the budget for all targets together, split
# evenly between them, so that adding a target cannot push a fixed-length run
# past the gate's timeout. The scheduled stage uses it.

cd "$(dirname "$0")/../fuzz"

# cargo-fuzz needs a nightly toolchain for the sanitizer and coverage flags it
# passes to rustc, which is why this is not part of the pinned-toolchain build.
# Every target is built before any runs, so the build is not charged to the
# first target's budget.
targets=$(cargo +nightly fuzz list)
cargo +nightly fuzz build

if [[ -n "${FUZZ_TOTAL_SECONDS:-}" ]]; then
  count=$(wc -w <<<"$targets")
  seconds=$((FUZZ_TOTAL_SECONDS / count))
else
  seconds="${FUZZ_SECONDS:-20}"
fi

for target in $targets; do
  echo "--- $target (${seconds}s)"
  cargo +nightly fuzz run "$target" -- \
    -max_total_time="$seconds" \
    -print_final_stats=1
done
