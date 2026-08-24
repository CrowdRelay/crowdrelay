"""Contract tests for the operator brief — phase 16.

The brief already counted what happened. What it could not say was what the
agent did *alone*, what it is about to do, what is waiting on a human, what it
stopped and why, and what actually moved. An autonomous system that reports only
its successes is one whose gaps stay invisible until somebody goes looking.

The properties pinned here:

- "alone" means approved by policy rather than by a person, read from the row
  rather than inferred;
- every stopped reason is reported verbatim, never summarised into a category
  that merges two different fixes;
- an unmeasurable claim is reported as a gap, not as a result;
- every number in `moved` carries the strength of its claim.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
BOUNDARY = ROOT / "crates/crowdrelay-application/src/autopilot/control.rs"
INFRA = ROOT / "crates/crowdrelay-infra/src/autopilot/operations/chief.rs"
OPENAPI = ROOT / "openapi/openapi.yaml"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


class OperatorBriefV2Contract(unittest.TestCase):
    def setUp(self) -> None:
        self.boundary = read(BOUNDARY)
        self.infra = read(INFRA)
        self.openapi = read(OPENAPI)

    def sections(self) -> list[str]:
        return [
            "acted_alone_24h",
            "about_to_act",
            "parked_for_approval",
            "stopped",
            "moved",
        ]

    def test_the_brief_answers_all_five_questions(self) -> None:
        block = self.boundary.split("pub struct AutopilotChiefOfStaff", 1)[1].split(
            "\n}", 1
        )[0]
        for section in self.sections():
            self.assertIn(section, block)
        schema = self.openapi.split("AutopilotChiefOfStaff:", 1)[1].split(
            "ChiefOfStaffActivity:", 1
        )[0]
        required = re.search(r"required: \[(.*?)\]", schema)
        self.assertIsNotNone(required)
        for section in self.sections():
            self.assertIn(section, required.group(1))

    def test_acting_alone_is_read_from_the_row_not_inferred(self) -> None:
        # "Alone" is the distinction an operator is checking for: approved by
        # policy rather than by a person.
        activity = self.infra.split("async fn chief_activity", 1)[1]
        self.assertIn("action.approved_by = 'policy:bounded_auto'", activity)
        self.assertIn("'acted_alone'", activity)
        self.assertIn("'parked'", activity)

    def test_the_three_activity_sections_come_from_one_read(self) -> None:
        # Three round trips would let the sections disagree about an action that
        # changed state between them.
        activity = self.infra.split("async fn chief_activity", 1)[1].split(
            "async fn chief_stopped", 1
        )[0]
        self.assertEqual(
            activity.count("sqlx::query_as"), 1, "one query partitions all three"
        )

    def test_every_stopped_reason_is_reported_verbatim(self) -> None:
        stopped = self.infra.split("async fn chief_stopped", 1)[1].split(
            "async fn chief_movements", 1
        )[0]
        # Grouped by the stored reason, never rewritten into a category.
        self.assertIn("step.skip_reason", stopped)
        self.assertIn("action.last_error_kind", stopped)
        self.assertIn("learning.retired_reason", stopped)
        self.assertIn("outcome.evidence_reason", stopped)
        for kind in (
            "play_step_skipped",
            "action_failed",
            "play_retired",
            "outcome_insufficient",
        ):
            self.assertIn(f"'{kind}'", stopped)
            self.assertIn(kind, self.openapi)

    def test_an_unmeasurable_claim_is_a_gap_and_not_a_result(self) -> None:
        stopped = self.infra.split("async fn chief_stopped", 1)[1].split(
            "async fn chief_movements", 1
        )[0]
        self.assertIn("outcome.evidence = 'insufficient'", stopped)
        moved = self.infra.split("async fn chief_movements", 1)[1]
        self.assertIn("outcome.evidence = 'measured'", moved)
        self.assertIn("outcome.effect_assessment IS NOT NULL", moved)

    def test_every_number_that_moved_carries_the_strength_of_its_claim(self) -> None:
        moved = self.infra.split("async fn chief_movements", 1)[1]
        self.assertIn("outcome.claim", moved)
        schema = self.openapi.split("ChiefOfStaffMovement:", 1)[1].split(
            "\n    ExecutorCapability:", 1
        )[0]
        self.assertIn("claim: { $ref: '#/components/schemas/PlayClaimStrength' }", schema)

    def test_every_section_is_bounded(self) -> None:
        # A brief that can grow without limit is a brief nobody reads.
        for section in self.sections():
            block = self.openapi.split(f"        {section}:", 1)[1].split("items:", 1)[0]
            self.assertIn("maxItems:", block)
        for query in ("chief_activity", "chief_stopped", "chief_movements"):
            self.assertIn("LIMIT", self.infra.split(f"async fn {query}", 1)[1][:4000])


if __name__ == "__main__":
    unittest.main()
