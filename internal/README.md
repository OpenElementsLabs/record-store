# Internal engineering notes

Everything under `internal/` is **internal engineering material**: architecture
notes, failure models, runbooks, test evidence, and known limitations.

It is deliberately **outside the documentation build**. MkDocs is configured with
the default `docs_dir: docs`, so nothing here is rendered, published to GitHub
Pages, or reachable from the public site. `mkdocs build --strict` never reads
this directory.

Nothing in here is a public claim. In particular, **clustering is not a
documented, publicly supported feature**, and the public documentation must
continue to say so until the release gates in
[`cluster/release-gates.md`](cluster/release-gates.md) are met.

## Contents

| Path | What it holds |
| --- | --- |
| [`cluster/failure-model.md`](cluster/failure-model.md) | The failure model and the guarantees the cluster actually enforces |
| [`cluster/runbooks.md`](cluster/runbooks.md) | Operator procedures: failover, repair, node removal, recovery |
| [`cluster/failure-matrix.md`](cluster/failure-matrix.md) | Every invariant mapped to the test that reproduces it |
| [`cluster/test-evidence.md`](cluster/test-evidence.md) | Suite results and measurements, with what they do not show |
| [`cluster/limitations.md`](cluster/limitations.md) | Known gaps, unsupported scenarios, and open risks |
| [`cluster/release-gates.md`](cluster/release-gates.md) | What must be true before clustering can be announced |
| [`cluster/gates.toml`](cluster/gates.toml) | The executable cluster readiness matrix, evaluated separately from the public standalone gates (`release/gates.toml`) |
