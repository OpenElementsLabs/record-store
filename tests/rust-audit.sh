#!/usr/bin/env bash
set -euo pipefail

# A yanked crate is not yet a vulnerability, but it is an upstream author saying
# "do not use this build". Treating warnings as failures keeps one out of
# Cargo.lock instead of leaving it to be noticed when it becomes an advisory.
cargo audit --deny warnings
