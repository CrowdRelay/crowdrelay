"""Contract tests for play measurement — phase 14.

The unit of measurement is the play. Measuring one send would credit a whole
campaign to whichever message happened to be last, and reading the success
metric without a frozen pre-play baseline would compare a play against a number
the play has already moved.

Two claims exist and must never merge. `attributed` is joined to the play
through our own rows. `correlational` is a series that moved over the play's
window with nothing joining the two. The strength travels with the number, in
the response, on every claim.

The third answer is what makes the other two worth anything: when a claim cannot
be made, it is stored and returned as `insufficient` with a reason. The
properties pinned here are the ones whose absence would quietly turn a
coincidence into a cause:

- the database refuses a verdict without evidence, and an insufficiency without
  a reason;
- every reason the rule can emit is a reason the table accepts;
- the attributed claim never borrows the correlational number, and a missing
  join key is never reported as zero;
- a campaign that reached nobody is a non-event, not a null result;
- the window is read as of its own end, so a late worker does not measure the
  weeks after the campaign and call them the campaign.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
MIGRATION = ROOT / "migrations/0089_viryaos_play_outcomes.sql"
DOMAIN = ROOT / "crates/crowdrelay-domain/src/play_measurement.rs"
PLAYS_DOMAIN = ROOT / "crates/crowdrelay-domain/src/plays.rs"
PORTS = ROOT / "crates/crowdrelay-application/src/autopilot/ports.rs"
LEDGER_PORTS = (
    ROOT / "crates/crowdrelay-application/src/autopilot/control/play_ports.rs"
)
INFRA = ROOT / "crates/crowdrelay-infra/src/autopilot/play_outcomes.rs"
PLAYS_INFRA = ROOT / "crates/crowdrelay-infra/src/autopilot/plays.rs"
EXECUTION = ROOT / "crates/crowdrelay-infra/src/autopilot/execution.rs"
WORKER = ROOT / "crates/crowdrelay-worker/src/autopilot.rs"
ROUTING = ROOT / "crates/crowdrelay-api/src/routing.rs"
OPENAPI = ROOT / "openapi/openapi.yaml"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def strip_sql_comments(sql: str) -> str:
    return "\n".join(line.split("--", 1)[0] for line in sql.splitlines())


class PlayMeasurementContract(unittest.TestCase):
    def setUp(self) -> None:
        self.migration = read(MIGRATION)
        self.sql = strip_sql_comments(self.migration)
        self.domain = read(DOMAIN)
        self.infra = read(INFRA)

    def reasons(self) -> set[str]:
        block = self.domain.split("impl InsufficientEvidence", 1)[1].split(
            "\n    }", 1
        )[0]
        return set(re.findall(r'Self::\w+ => "([a-z_]+)"', block))

    # --- the two claims -------------------------------------------------

    def test_both_claims_are_opened_when_the_play_starts(self) -> None:
        # An absent row is invisible. A row that says the claim cannot be made,
        # and why, is the whole point of measuring.
        self.assertIn("pub const fn all() -> [Self; 2]", self.domain)
        self.assertIn("for claim in PlayClaim::all()", self.infra)
        self.assertIn("UNIQUE (workspace_id, play_id, claim)", self.sql)
        self.assertIn("claim IN ('attributed', 'correlational')", self.sql)

    def test_the_strength_of_a_claim_travels_with_the_number(self) -> None:
        ledger = read(LEDGER_PORTS)
        self.assertIn("pub claim: PlayClaim", ledger)
        self.assertIn("pub claim_means: &'static str", ledger)
        self.assertIn("claim_means: claim.description()", self.infra)
        openapi = read(OPENAPI)
        schema = openapi.split("PlayClaimView:", 1)[1].split("PlayLedgerEntry:", 1)[0]
        required = re.search(r"required: \[(.*?)\]", schema)
        self.assertIsNotNone(required)
        for field in ("claim", "claim_means"):
            self.assertIn(field, required.group(1))

    def test_the_attributed_claim_never_borrows_the_other_number(self) -> None:
        rule = self.domain.split("pub fn assess_play_outcome", 1)[1]
        attributed = rule.split("PlayClaim::Attributed", 1)[1].split(
            "PlayClaim::Correlational", 1
        )[0]
        self.assertIn("attributed_clicks", attributed)
        self.assertIn("NoAttributionKey", attributed)
        # And the adapter says "no join key" rather than "zero clicks".
        self.assertIn("attributed_clicks: None", self.infra)

    # --- what the database refuses --------------------------------------

    def test_a_verdict_cannot_be_stored_without_evidence(self) -> None:
        self.assertIn(
            "CHECK (effect_assessment IS NULL OR evidence IS NOT DISTINCT FROM 'measured')",
            self.sql,
        )
        self.assertIn(
            "CHECK (delta_basis_points IS NULL OR effect_assessment IS NOT NULL)",
            self.sql,
        )

    def test_an_insufficiency_names_its_reason_and_only_an_insufficiency_has_one(
        self,
    ) -> None:
        # Without both halves, "we could not tell" and "we did not look" are the
        # same row.
        self.assertIn(
            "CHECK ((evidence IS NOT DISTINCT FROM 'insufficient') = (evidence_reason IS NOT NULL))",
            self.sql,
        )

    def test_the_evidence_constraints_are_null_safe(self) -> None:
        # A CHECK whose expression evaluates to NULL passes. On an unsettled row
        # `evidence` is NULL, so `evidence = 'measured'` is NULL and a plain `=`
        # waves through precisely the shape the constraint exists to forbid: a
        # verdict with nothing behind it. Found by a Postgres test, not by
        # reading the SQL.
        for constraint in ("reason_matches_evidence", "verdict_requires_evidence"):
            clause = self.sql.split(f"viryaos_play_outcomes_{constraint}", 1)[1].split(
                "\n", 2
            )[1]
            self.assertIn("IS NOT DISTINCT FROM", clause)

    def test_a_settled_outcome_and_its_evidence_cannot_disagree(self) -> None:
        self.assertIn(
            "CHECK ((status = 'succeeded') = (evidence IS NOT NULL))", self.sql
        )

    def test_every_reason_the_rule_emits_is_a_reason_the_table_accepts(self) -> None:
        stored = re.search(
            r"evidence_reason IS NULL OR evidence_reason IN \((.*?)\)",
            self.sql,
            re.DOTALL,
        )
        self.assertIsNotNone(stored)
        self.assertEqual(set(re.findall(r"'([a-z_]+)'", stored.group(1))), self.reasons())

    # --- what is never invented -----------------------------------------

    def test_a_campaign_that_reached_nobody_is_a_non_event(self) -> None:
        rule = self.domain.split("pub fn assess_play_outcome", 1)[1]
        reach = rule.index("recipients_reached == 0")
        claims = rule.index("match input.claim")
        self.assertLess(reach, claims, "reach is checked before either claim speaks")
        self.assertIn("NothingDelivered", rule[reach:claims])

    def test_a_flat_baseline_withholds_the_percentage_rather_than_inventing_one(
        self,
    ) -> None:
        self.assertIn("fn relative_delta", self.domain)
        self.assertIn("minimum_baseline_milli_per_day", self.domain)
        self.assertIn(
            "a_flat_baseline_yields_a_verdict_but_never_an_invented_percentage",
            self.domain,
        )

    def test_two_series_answering_to_one_metric_are_refused_not_added(self) -> None:
        self.assertIn("AmbiguousSeries", self.domain)
        # The adapter reads two and stops, rather than picking or summing.
        series = self.infra.split("pub(super) async fn read_series", 1)[1]
        self.assertIn("LIMIT 2", series)
        self.assertIn("ambiguous: true", series)

    def test_the_reach_denominator_comes_from_delivery_not_from_intent(self) -> None:
        # An action that was queued and never sent reached nobody. Counting it
        # would give every number here a denominator larger than the truth.
        observe = self.infra.split("observe_play_outcome_impl", 1)[1]
        self.assertIn("viryaos_play_step_recipients", observe)
        self.assertNotIn("viryaos_autopilot_actions", observe.split("Ok(PlayOutcome", 1)[0])

    # --- when the numbers are taken -------------------------------------

    def test_the_baseline_is_frozen_in_the_transaction_that_creates_the_play(
        self,
    ) -> None:
        start = read(PLAYS_INFRA).split("start_play_impl", 1)[1]
        start = start.split("pub(super) async fn", 1)[0]
        opened = start.index("open_play_outcomes")
        # The last commit in the function is the one that creates the play. The
        # earlier one is the "a play already covered this anchor" return, which
        # has nothing to open.
        committed = start.rindex("transaction.commit()")
        self.assertLess(
            opened,
            committed,
            "a play committed without its baseline can never be measured honestly",
        )

    def test_the_window_is_read_as_of_its_own_end(self) -> None:
        observe = self.infra.split("observe_play_outcome_impl", 1)[1]
        self.assertIn("outcome.window_end,", observe)
        self.assertIn("observed_at: outcome.window_end", observe)
        # And a window that has not closed is not claimed at all.
        self.assertIn("outcome.window_end <= $2", self.infra)

    def test_the_window_closes_after_the_last_step_plus_a_settle_period(self) -> None:
        self.assertIn("pub fn measurement_due_at", self.domain)
        self.assertIn("settle_days", self.domain)
        self.assertIn("pub measurement: PlayMeasurementPolicy", read(PLAYS_DOMAIN))

    def test_measurement_settles_even_when_the_context_is_switched_off(self) -> None:
        # Measuring what already happened is not acting on it. A campaign that
        # ran before an operator paused the agent still deserves an answer.
        worker = read(WORKER)
        self.assertIn("claim_due_play_outcomes", worker)
        self.assertIn("assess_play_claim", worker)
        self.assertIn("fail_play_outcome", worker)

    def test_a_play_step_schedules_no_action_level_measurement(self) -> None:
        # The two measurement systems stay separate: one measures an action
        # against a metric it moved directly, this one measures a campaign.
        plans = read(EXECUTION).split("pub(super) async fn schedule_effect_measurement", 1)[1]
        plans = plans.split("for (kind, subject_id", 1)[0]
        skipped = plans.split("=> {}", 1)[0]
        self.assertIn("AutopilotActionPayload::RunPlayStep { .. }", skipped)

    def test_the_ledger_is_readable_and_published(self) -> None:
        self.assertIn('"/v1/admin/autopilot/plays"', read(ROUTING))
        openapi = read(OPENAPI)
        self.assertIn("/admin/autopilot/plays:", openapi)
        self.assertIn("PlayLedgerResponse", openapi)
        published = re.search(
            r"PlayEvidenceReason:.*?enum: \[(.*?)\]", openapi, re.DOTALL
        )
        self.assertIsNotNone(published)
        self.assertEqual(
            {value.strip() for value in published.group(1).split(",")}, self.reasons()
        )


if __name__ == "__main__":
    unittest.main()
