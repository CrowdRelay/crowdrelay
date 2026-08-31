"""Contract tests for read-model integrity in decision_evidence / learning_loop.

Enforces three invariants that the substrate hardening sprint established:

1. decision_evidence must be workspace-scoped (tenant isolation).
2. learning_loop must not fabricate defaults for missing persisted truth.
3. learning_loop must use LATERAL LIMIT 1 for actions to prevent join fan-out.

Also enforces that the read model distinguishes absent entities from corrupt
entities via stage-specific data_integrity warnings (action and outcome are
independent — action corruption does NOT mark outcome corrupt).
"""

import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
EVIDENCE = ROOT / "crates/crowdrelay-api/src/autopilot/decision_evidence.rs"


class ReadModelIntegrityContract(unittest.TestCase):
    def test_decision_evidence_is_workspace_scoped(self):
        """decision_evidence must filter by workspace_id, not just decision_id."""
        text = EVIDENCE.read_text()
        self.assertIn("workspace_id", text, "workspace_id must appear in the file")
        self.assertIn(
            "AND workspace_id",
            text,
            "decision_evidence query must include AND workspace_id predicate",
        )

    def test_learning_loop_has_no_fabricated_defaults(self):
        """learning_loop must not fabricate defaults for missing data."""
        text = EVIDENCE.read_text()
        for forbidden in (
            "unwrap_or_default()",
            "unwrap_or(0)",
            "unwrap_or_else(OffsetDateTime::now_utc)",
        ):
            self.assertNotIn(
                forbidden,
                text,
                f"read model must not fabricate defaults: {forbidden}",
            )

    def test_learning_loop_action_cardinality_is_bounded(self):
        """learning_loop must use LATERAL LIMIT 1 for actions to prevent fan-out."""
        text = EVIDENCE.read_text()
        self.assertIn("LATERAL", text, "learning loop must use LATERAL joins")
        self.assertIn(
            "LIMIT 1",
            text,
            "learning loop must use LIMIT 1 in LATERAL subqueries",
        )

    def test_data_integrity_warnings_are_stage_specific(self):
        """LearningLoopEntry must have stage-specific data_integrity warnings
        (action and outcome are independent). The old single
        data_integrity_warning field must NOT exist."""
        text = EVIDENCE.read_text()
        self.assertIn(
            "data_integrity",
            text,
            "LearningLoopEntry must have data_integrity field",
        )
        self.assertIn(
            "DataIntegrityWarnings",
            text,
            "DataIntegrityWarnings struct must exist",
        )
        self.assertIn(
            "pub action: Option<String>",
            text,
            "DataIntegrityWarnings must have stage-specific action field",
        )
        self.assertIn(
            "pub outcome: Option<String>",
            text,
            "DataIntegrityWarnings must have stage-specific outcome field",
        )
        # The old single-field data_integrity_warning must NOT exist
        self.assertNotIn(
            "data_integrity_warning",
            text,
            "old single data_integrity_warning field must be removed",
        )


if __name__ == "__main__":
    unittest.main()
