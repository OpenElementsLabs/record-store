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

seconds="${FUZZ_SECONDS:-20}"
cd "$(dirname "$0")/../fuzz"

# cargo-fuzz needs a nightly toolchain for the sanitizer and coverage flags it
# passes to rustc, which is why this is not part of the pinned-toolchain build.
for target in $(cargo +nightly fuzz list); do
  echo "--- $target (${seconds}s)"
  cargo +nightly fuzz run "$target" -- \
    -max_total_time="$seconds" \
    -print_final_stats=1
done
