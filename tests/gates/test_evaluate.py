"""The evaluator must never report READY on evidence that does not support it."""

from __future__ import annotations

import datetime
import json
import subprocess
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
from record import definition_digest  # noqa: E402

COMMIT = "a" * 40
BINARIES = {"record-store": "1" * 64, "record-store-server": "2" * 64}
MATRIX = """
schema = 1
matrix_version = "test"
scope = "standalone"
[stages]
candidate = "c"
pr = "p"
[[gate]]
id = "G-PLAIN"
title = "t"
area = "correctness"
guarantee = "g"
modes = ["standalone"]
platforms = ["any"]
stages = ["candidate", "pr"]
blocking = true
command = "true"
environment = "e"
fixtures = "f"
failure_injection = "none"
evidence = "e"
implementation = []
timeout_minutes = 1
limitations = "l"
binds_artifact = false
[[gate]]
id = "G-BOUND"
title = "t"
area = "recovery"
guarantee = "g"
modes = ["standalone"]
platforms = ["any"]
stages = ["candidate", "pr"]
blocking = true
command = "run --profile ${PROFILE}"
environment = "e"
fixtures = "f"
failure_injection = "none"
evidence = "e"
implementation = []
timeout_minutes = 1
limitations = "l"
binds_artifact = true
[[gate]]
id = "G-ADVISORY"
title = "t"
area = "performance"
guarantee = "g"
modes = ["standalone"]
platforms = ["any"]
stages = ["candidate", "pr"]
blocking = false
command = "true"
environment = "e"
fixtures = "f"
failure_injection = "none"
evidence = "e"
implementation = []
timeout_minutes = 1
limitations = "l"
binds_artifact = false
"""


class EvaluatorTest(unittest.TestCase):
    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp())
        (self.root / "gates.toml").write_text(MATRIX)
        self.matrix = tomllib.loads(MATRIX)
        self.results = self.root / "results"
        self.results.mkdir()
        self.exceptions = self.root / "exceptions"
        self.exceptions.mkdir()
        self.quarantine = self.root / "quarantine.toml"
        self.quarantine.write_text("")
        self.findings = self.root / "findings"
        self.findings.mkdir()
        candidate = self.root / "candidate"
        candidate.mkdir()
        (candidate / "candidate.json").write_text(json.dumps({"commit": COMMIT, "binaries": BINARIES}))
        self.candidate = candidate

    def gate(self, gate_id: str) -> dict:
        return next(g for g in self.matrix["gate"] if g["id"] == gate_id)

    def result(self, gate_id: str, status: str = "pass", **overrides) -> None:
        record = {
            "schema": 1, "gate": gate_id, "status": status, "commit": COMMIT,
            "definition_digest": definition_digest(self.gate(gate_id), self.root),
            "candidate": {"binaries": BINARIES}, "finished_at": "2026-09-23T00:00:00Z",
            "attempts": [{"outcome": status}], "checks": {"failed": []}, "notes": [], "profile": "candidate",
        }
        record.update(overrides)
        (self.results / f"{gate_id}.json").write_text(json.dumps(record))

    def evaluate(self, *extra: str, stage: str = "candidate") -> tuple[int, dict]:
        out = self.root / "report"
        process = subprocess.run(
            [sys.executable, str(HERE / "evaluate.py"), "--matrix", str(self.root / "gates.toml"), "--stage", stage,
             "--candidate", str(self.candidate), "--results", str(self.results), "--exceptions", str(self.exceptions),
             "--quarantine", str(self.quarantine), "--findings", str(self.findings), "--commit", COMMIT,
             "--out", str(out), *extra],
            capture_output=True, text=True)
        report = json.loads((out / "report.json").read_text()) if (out / "report.json").exists() else {}
        return process.returncode, report

    def all_pass(self) -> None:
        for gate_id in ("G-PLAIN", "G-BOUND", "G-ADVISORY"):
            self.result(gate_id)

    def test_every_gate_passing_is_ready(self) -> None:
        self.all_pass()
        code, report = self.evaluate()
        self.assertEqual((code, report["decision"]), (0, "READY"))

    def test_a_gate_that_never_ran_blocks(self) -> None:
        self.result("G-PLAIN")
        self.result("G-ADVISORY")
        code, report = self.evaluate()
        self.assertEqual(code, 1)
        self.assertIn("G-BOUND", report["blockers"])

    def test_a_skipped_gate_is_not_a_pass(self) -> None:
        self.all_pass()
        self.result("G-PLAIN", "skipped")
        self.assertEqual(self.evaluate()[1]["decision"], "BLOCKED")

    def test_infrastructure_errors_and_invalid_measurements_block(self) -> None:
        for status in ("infrastructure_error", "invalid_measurement"):
            self.all_pass()
            self.result("G-PLAIN", status)
            self.assertEqual(self.evaluate()[1]["decision"], "BLOCKED", status)

    def test_a_result_from_another_gate_definition_does_not_count(self) -> None:
        self.all_pass()
        self.result("G-PLAIN", definition_digest="0" * 64)
        code, report = self.evaluate()
        self.assertEqual(code, 1)
        row = next(r for r in report["gates"] if r["gate"] == "G-PLAIN")
        self.assertEqual(row["status"], "stale")

    def test_a_result_for_other_binaries_does_not_count(self) -> None:
        self.all_pass()
        self.result("G-BOUND", candidate={"binaries": {"record-store": "9" * 64, "record-store-server": "9" * 64}})
        self.assertIn("G-BOUND", self.evaluate()[1]["blockers"])

    def test_a_result_for_another_commit_is_not_reused_without_permission(self) -> None:
        self.all_pass()
        self.result("G-PLAIN", commit="b" * 40)
        self.assertIn("G-PLAIN", self.evaluate()[1]["blockers"])

    def test_a_failing_advisory_gate_is_listed_but_does_not_block(self) -> None:
        self.all_pass()
        self.result("G-ADVISORY", "fail")
        code, report = self.evaluate()
        self.assertEqual((code, report["decision"]), (0, "READY"))
        self.assertIn("G-ADVISORY", report["advisory_not_passing"])

    def test_a_complete_current_exception_waives_one_gate_visibly(self) -> None:
        self.all_pass()
        self.result("G-PLAIN", "fail")
        expires = (datetime.date.today() + datetime.timedelta(days=7)).isoformat()
        (self.exceptions / "E1.toml").write_text(
            f'id = "E1"\ngate = "G-PLAIN"\nguarantee = "g"\nevidence = "e"\nrisk = "r"\nowner = "o"\n'
            f'approved_by = "a"\ncreated = 2026-09-01\nexpires = {expires}\n')
        code, report = self.evaluate()
        self.assertEqual((code, report["decision"]), (0, "READY WITH EXCEPTIONS"))
        self.assertEqual(report["excepted"], ["G-PLAIN"])

    def test_an_expired_exception_waives_nothing(self) -> None:
        self.all_pass()
        self.result("G-PLAIN", "fail")
        (self.exceptions / "E1.toml").write_text(
            'id = "E1"\ngate = "G-PLAIN"\nguarantee = "g"\nevidence = "e"\nrisk = "r"\nowner = "o"\n'
            'approved_by = "a"\ncreated = 2026-01-01\nexpires = 2026-01-02\n')
        self.assertEqual(self.evaluate()[1]["decision"], "BLOCKED")

    def test_an_incomplete_exception_is_an_error_not_a_waiver(self) -> None:
        self.all_pass()
        self.result("G-PLAIN", "fail")
        (self.exceptions / "E1.toml").write_text('id = "E1"\ngate = "G-PLAIN"\nexpires = 2099-01-01\n')
        self.assertEqual(self.evaluate()[0], 2)

    def test_an_expired_quarantine_entry_blocks(self) -> None:
        self.all_pass()
        self.quarantine.write_text(
            '[[entry]]\ntest = "x.rs::t"\nowner = "o"\nreason = "r"\nexpires = 2026-01-01\n'
            'coverage_risk = "c"\nissue = "i"\n')
        self.assertIn("QUARANTINE", self.evaluate()[1]["blockers"])

    def test_a_candidate_built_from_another_commit_blocks(self) -> None:
        self.all_pass()
        (self.candidate / "candidate.json").write_text(json.dumps({"commit": "c" * 40, "binaries": BINARIES}))
        self.assertIn("CANDIDATE-COMMIT", self.evaluate()[1]["blockers"])

    def test_an_open_release_blocking_finding_blocks(self) -> None:
        self.all_pass()
        (self.findings / "RSG-900-x.md").write_text("| | |\n| --- | --- |\n| Status | open |\n| Blocks release | yes |\n")
        (self.findings / "RSG-901-y.md").write_text("| | |\n| --- | --- |\n| Status | fixed |\n| Blocks release | yes |\n")
        (self.findings / "RSG-902-z.md").write_text("| | |\n| --- | --- |\n| Status | open |\n| Blocks release | no |\n")
        report = self.evaluate()[1]
        self.assertIn("FINDINGS", report["blockers"])
        row = next(r for r in report["gates"] if r["gate"] == "FINDINGS")
        self.assertEqual(row["failed_checks"], ["RSG-900"])

    def test_a_deferred_gate_is_never_reported_as_ready(self) -> None:
        self.result("G-PLAIN")
        self.result("G-ADVISORY")
        code, report = self.evaluate("--defer", "G-BOUND")
        self.assertEqual((code, report["decision"]), (0, "READY SO FAR"))
        self.assertEqual(report["deferred"], ["G-BOUND"])
        self.assertEqual(self.evaluate()[1]["decision"], "BLOCKED")

    def test_a_lighter_profile_is_not_evidence_for_a_heavier_stage(self) -> None:
        self.all_pass()
        self.result("G-BOUND", profile="pr")
        self.assertIn("G-BOUND", self.evaluate()[1]["blockers"])

    def known(self) -> None:
        (self.findings / "RSG-900-x.md").write_text(
            "| | |\n| --- | --- |\n| Status | open |\n| Blocks release | yes |\n| Gate | G-PLAIN (x) |\n"
            "| Known failing checks | `flipped byte`; `crc32` |\n")

    def test_a_tracked_failure_does_not_block_a_pull_request_but_blocks_a_release(self) -> None:
        self.all_pass()
        self.result("G-PLAIN", "fail", checks={"failed": ["one flipped byte fails", "crc32 is refused"]})
        self.result("G-BOUND", profile="pr")
        self.known()
        code, report = self.evaluate(stage="pr")
        self.assertEqual((code, report["decision"]), (0, "READY"), report["blockers"])
        self.assertEqual(report["known_failures"], ["G-PLAIN"])
        self.assertIn("G-PLAIN", self.evaluate()[1]["blockers"])
        self.assertIn("FINDINGS", self.evaluate()[1]["blockers"])
        self.assertIn("FINDINGS", self.evaluate("--enforce-findings", stage="pr")[1]["blockers"])

    def test_a_new_failure_in_a_tracked_gate_still_blocks_a_pull_request(self) -> None:
        self.all_pass()
        self.result("G-PLAIN", "fail", checks={"failed": ["one flipped byte fails", "something new broke"]})
        self.result("G-BOUND", profile="pr")
        self.known()
        self.assertIn("G-PLAIN", self.evaluate(stage="pr")[1]["blockers"])

    def test_a_malformed_matrix_is_an_error(self) -> None:
        (self.root / "gates.toml").write_text(MATRIX.replace('guarantee = "g"\n', "", 1))
        self.assertEqual(self.evaluate()[0], 2)


if __name__ == "__main__":
    unittest.main()
