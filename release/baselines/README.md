# Performance baselines

`PERF-BASELINE` compares a candidate against the baseline named by
`[gate.baseline] path` in `release/gates.toml`, for the same environment key
(OS, architecture, CPU count and model). A baseline is a file here, produced by

```bash
python3 tests/gates/perf.py --bin-dir <candidate>/bin --profile scheduled \
  --record-baseline release/baselines/<environment-key>.json
```

on the reference environment (GitHub-hosted `ubuntu-24.04`), from the scheduled
workflow's evidence. It takes effect only when a reviewed change points
`gates.toml` at it: nothing updates a baseline as a side effect of a run, and
`--record-baseline` refuses to overwrite an existing file. Replacing a baseline
with a slower one is a decision to accept a regression, and the pull request
that does it should say so.

No baseline is committed at matrix `2026.09.23-1`, so the regression half of
`PERF-BASELINE` reports an invalid measurement and blocks the candidate stage
until the first scheduled run has been reviewed and recorded.
