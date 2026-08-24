"""Contract tests for play learning — phase 15.

Every context before this decides from the present. Nothing carried a memory of
whether a kind of campaign has ever worked, so a play that measured `worsened`
three times running was proposed exactly as often as one that measured
`improved`.

The properties pinned here are the ones whose absence would turn a bounded,
explainable record into a model nobody can argue with — or, worse, into a way
for the agent to widen its own authority:

- an outcome nobody could measure counts neither for nor against;
- one bad result changes nothing;
- a record may only narrow reach, never widen anything, and never touches the
  class ceiling, the context ladder or the growth envelope;
- retirement is a stated fact with a reason, not a weight that decayed to zero;
- an operator's retirement is never presented as the agent's own conclusion.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
MIGRATION = ROOT / "migrations/0091_viryaos_play_learning.sql"
DOMAIN = ROOT / "crates/crowdrelay-domain/src/learning.rs"
PLAYS_DOMAIN = ROOT / "crates/crowdrelay-domain/src/plays.rs"
EVALUATE = ROOT / "crates/crowdrelay-application/src/autopilot/evaluate.rs"
PORTS = ROOT / "crates/crowdrelay-application/src/autopilot/ports.rs"
INFRA = ROOT / "crates/crowdrelay-infra/src/autopilot/play_outcomes.rs"
OPENAPI = ROOT / "openapi/openapi.yaml"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def strip_sql_comments(sql: str) -> str:
    return "\n".join(line.split("--", 1)[0] for line in sql.splitlines())


class PlayLearningContract(unittest.TestCase):
    def setUp(self) -> None:
        self.sql = strip_sql_comments(read(MIGRATION))
        self.domain = read(DOMAIN)
        self.infra = read(INFRA)

    def reasons(self) -> set[str]:
        block = self.domain.split("impl RetirementReason", 1)[1].split("\n    }", 1)[0]
        return set(re.findall(r'Self::\w+ => "([a-z_]+)"', block))

    # --- what the record is allowed to conclude -------------------------

    def test_an_unmeasurable_outcome_counts_neither_way(self) -> None:
        # Letting it count would retire the plays the agent cannot see rather
        # than the ones that do not work.
        observe = self.domain.split("pub const fn observe", 1)[1].split("\n    }", 1)[0]
        none_arm = observe.split("None => Self", 1)[1]
        self.assertIn("insufficient", none_arm)
        self.assertNotIn("consecutive_worsened", none_arm)
        measured = self.domain.split("pub const fn measured", 1)[1].split("\n    }", 1)[0]
        self.assertNotIn("insufficient", measured)
        self.assertIn(
            "an_unmeasurable_outcome_neither_breaks_nor_extends_a_run_of_failures",
            self.domain,
        )

    def test_one_bad_result_changes_nothing(self) -> None:
        self.assertIn("minimum_measured_record", self.domain)
        self.assertIn("PlayStanding::Untested", self.domain)
        self.assertIn("one_bad_result_changes_nothing", self.domain)

    def test_a_record_may_only_narrow(self) -> None:
        ceiling = self.domain.split("pub fn effective_recipient_ceiling", 1)[1].split(
            "\n}", 1
        )[0]
        # Scaling by a weight capped at 10_000 can only reduce the operator's
        # own number.
        self.assertIn("/ 10_000", ceiling)
        self.assertIn("a_perfect_record_never_widens_anything", self.domain)
        self.assertIn(
            "weight_basis_points integer NOT NULL DEFAULT 10000\n        CHECK (weight_basis_points BETWEEN 0 AND 10000)",
            self.sql,
        )

    def test_the_record_never_touches_authority(self) -> None:
        # The class ceiling, the context ladder and the envelope are somebody
        # else's to move, however good a record looks.
        for authority in ("ActionClass", "AutonomyLevel", "GrowthEnvelope", "clamp_disposition"):
            self.assertNotIn(authority, self.domain)
        for table in (
            "viryaos_autopilot_authority",
            "viryaos_growth_envelope",
            "viryaos_autopilot_policies",
        ):
            self.assertNotIn(f"UPDATE {table}", self.infra)

    # --- retirement -----------------------------------------------------

    def test_retirement_is_a_stated_fact_with_a_reason(self) -> None:
        self.assertIn(
            "CHECK ((retired_at IS NULL) = (retired_reason IS NULL))", self.sql
        )
        # A zero weight without a retirement would be a silent stop no read
        # model could explain.
        self.assertIn(
            "CHECK ((weight_basis_points = 0) = (retired_at IS NOT NULL))", self.sql
        )
        self.assertIn("a_running_play_always_reaches_somebody", self.domain)

    def test_every_reason_the_rule_emits_is_a_reason_the_table_accepts(self) -> None:
        stored = re.search(
            r"retired_reason IS NULL OR retired_reason IN \((.*?)\)", self.sql, re.DOTALL
        )
        self.assertIsNotNone(stored)
        self.assertEqual(set(re.findall(r"'([a-z_]+)'", stored.group(1))), self.reasons())

    def test_an_operator_retirement_is_never_the_agents_conclusion(self) -> None:
        rule = self.domain.split("pub fn assess_play_standing", 1)[1]
        operator = rule.index("operator_retired")
        self_retire = rule.index("RepeatedlyWorsened")
        self.assertLess(
            operator, self_retire, "a human's decision is checked before the agent's"
        )
        # And the adapter only ever reads that flag back; it never sets it.
        self.assertIn("RetirementReason::OperatorRetired.as_str()", self.infra)
        self.assertNotIn(
            "retired_reason = 'operator_retired'", self.infra
        )

    def test_a_run_of_failures_retires_before_the_sample_guard_forgives_it(self) -> None:
        rule = self.domain.split("pub fn assess_play_standing", 1)[1]
        retire = rule.index("policy.retire_after_consecutive_worsened")
        sample = rule.index("policy.minimum_measured_record")
        self.assertLess(
            retire,
            sample,
            "a play that harmed the number every time it was measured must not keep "
            "running for want of a larger sample",
        )

    # --- where it bites --------------------------------------------------

    def test_the_record_moves_with_the_outcome_or_not_at_all(self) -> None:
        complete = self.infra.split("complete_play_outcome_impl", 1)[1]
        self.assertIn("record_play_outcome", complete)
        recorded = complete.index("record_play_outcome")
        committed = complete.index("transaction.commit()")
        self.assertLess(recorded, committed)
        # Only the claim that yields a verdict feeds the record.
        self.assertIn("outcome.claim == PlayClaim::Correlational", complete)

    def test_a_retired_kind_is_proposed_no_longer(self) -> None:
        arm = read(EVALUATE).split("AutopilotContext::Plays =>", 1)[1]
        self.assertIn("load_play_standings", arm)
        self.assertIn("standing.standing.is_retired()", arm)
        # And a campaign already committed to a show still finishes, because
        # abandoning it mid-run would strand steps nothing ever settles.
        self.assertIn("effective_max_recipients_per_step", arm)
        self.assertIn("filter(|standing| !standing.standing.is_retired())", arm)

    def test_the_policy_is_the_operators(self) -> None:
        self.assertIn("pub learning: LearningPolicy", read(PLAYS_DOMAIN))
        self.assertIn("async fn load_play_standings(", read(PORTS))
        self.assertIn("fn play_learning_policy", self.infra)

    def test_the_standing_is_published_with_the_counts_behind_it(self) -> None:
        openapi = read(OPENAPI)
        self.assertIn("PlayKindStanding:", openapi)
        self.assertIn("PlayRecordCounts:", openapi)
        ledger = openapi.split("PlayLedgerResponse:", 1)[1].split("\n    Play", 1)[0]
        self.assertIn("standings", ledger)
        published = re.search(
            r"reason: \{ type: string, enum: \[(.*?)\] \}", openapi, re.DOTALL
        )
        self.assertIsNotNone(published)
        self.assertEqual(
            {value.strip() for value in published.group(1).split(",")}, self.reasons()
        )


if __name__ == "__main__":
    unittest.main()
