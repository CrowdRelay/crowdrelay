"""Contract tests for Phase 8c — talking terms on a live opportunity.

This is the first place the agent commits the band's calendar and its money in
one move, so the claims worth pinning are not about the arithmetic. They are
about the things that must hold when the arithmetic says yes.

Six refusals hold at every autonomy level, and none of them is a threshold an
operator can loosen: a signed contract, an exclusivity clause, a date that is
not free, a trip that could not be costed, a year already past its stretch, and
a stretch slot below the operator's own bar. A fee that clears the floor buys
none of them.

The rest of this file pins the two structural decisions the negotiation depends
on: the ladder is frozen at open but the acceptance re-reads the current cost,
and every outward move is `third_party` and therefore parked for a human.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
MIGRATION = ROOT / "migrations/0097_viryaos_team_opportunity_terms.sql"
DOMAIN = ROOT / "crates/crowdrelay-domain/src/negotiation.rs"
POLICY = ROOT / "crates/crowdrelay-domain/src/live_opportunities.rs"
MODEL = ROOT / "crates/crowdrelay-application/src/autopilot/model.rs"
CANDIDATE = ROOT / "crates/crowdrelay-application/src/autopilot/evaluate/candidates.rs"
EVALUATE = ROOT / "crates/crowdrelay-application/src/autopilot/evaluate.rs"
INFRA = ROOT / "crates/crowdrelay-infra/src/autopilot/terms.rs"
INGRESS = ROOT / "crates/crowdrelay-infra/src/autopilot/operations/ingress/team.rs"
EXECUTOR = ROOT / "crates/crowdrelay-infra/src/autopilot/operations/execution.rs"
EXECUTION = ROOT / "crates/crowdrelay-infra/src/autopilot/execution.rs"
EXECUTION_CAPS = ROOT / "crates/crowdrelay-infra/src/autopilot/execution_capabilities.rs"
OPENAPI = ROOT / "openapi/openapi.yaml"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def strip_sql_comments(sql: str) -> str:
    return "\n".join(line.split("--", 1)[0] for line in sql.splitlines())


class LiveOpportunityTermsContract(unittest.TestCase):
    def setUp(self) -> None:
        self.sql = strip_sql_comments(read(MIGRATION))
        self.domain = read(DOMAIN)
        self.infra = read(INFRA)

    # --- the refusals ----------------------------------------------------

    def test_the_refusals_are_an_enum_and_not_a_settings_row(self) -> None:
        # A threshold an operator can loosen is not a refusal. Every one of
        # these has to be unreachable from the policy struct.
        declared = set(
            re.findall(
                r'Self::\w+ => "([a-z_]+)"',
                self.domain.split("impl TermsRefusal", 1)[1].split("\n}", 1)[0],
            )
        )
        self.assertEqual(
            declared,
            {
                "below_floor",
                "requires_contract",
                "exclusive",
                "date_not_free",
                "past_annual_stretch",
                "stretch_score_too_low",
                "cost_insufficient",
            },
        )
        policy = read(POLICY).split("pub struct LiveOpportunityPolicy", 1)[1].split("\n}", 1)[0]
        for forbidden in ("allow_contract", "allow_exclusive", "skip_floor", "accept_below"):
            self.assertNotIn(forbidden, policy)

    def test_the_refusals_are_checked_before_the_money(self) -> None:
        # A fee that clears the floor on a show requiring a signed contract is
        # still a show requiring a signed contract.
        rule = self.domain.split("pub fn evaluate_terms", 1)[1].split("\n}", 1)[0]
        refusal = rule.index("terms_refusal(")
        accept = rule.index("TermsDecision::Accept")
        self.assertLess(refusal, accept)
        # And a closed window beats every offer, because terms agreed after the
        # promoter stopped waiting are not terms.
        self.assertLess(rule.index("TermsDecision::Expire"), refusal)

    def test_an_uncosted_trip_is_never_a_cleared_floor(self) -> None:
        refusals = self.domain.split("pub fn terms_refusal", 1)[1].split("\n}", 1)[0]
        self.assertIn("costed_from_logistics", refusals)
        self.assertIn("TermsRefusal::CostInsufficient", refusals)

    def test_the_stored_states_and_reasons_match_the_rust_enums(self) -> None:
        stored_states = re.search(r"state IN \((.*?)\)", self.sql, re.DOTALL)
        self.assertIsNotNone(stored_states)
        declared_states = set(
            re.findall(
                r'Self::\w+ => "([a-z_]+)"',
                self.domain.split("impl TermsState", 1)[1].split("pub fn parse", 1)[0],
            )
        )
        self.assertEqual(
            set(re.findall(r"'([a-z_]+)'", stored_states.group(1))), declared_states
        )
        stored_reasons = re.search(r"settled_reason IN \((.*?)\)", self.sql, re.DOTALL)
        self.assertIsNotNone(stored_reasons)
        stored_reasons = set(re.findall(r"'([a-z_]+)'", stored_reasons.group(1)))
        declared_reasons = set(
            re.findall(
                r'Self::\w+ => "([a-z_]+)"',
                self.domain.split("impl TermsRefusal", 1)[1].split("\n}", 1)[0],
            )
        )
        # The two the domain cannot produce: nobody decided anything, so they
        # are not refusals.
        self.assertEqual(stored_reasons - declared_reasons, {"promoter_withdrew", "window_closed"})
        self.assertEqual(declared_reasons - stored_reasons, set())

    # --- the ladder ------------------------------------------------------

    def test_the_floor_is_cost_plus_margin_plus_the_application_fee(self) -> None:
        ladder = self.domain.split("pub fn terms_ladder", 1)[1].split("\n}", 1)[0]
        self.assertIn("policy.minimum_margin_minor", ladder)
        self.assertIn("snapshot.application_fee_minor", ladder)
        # And only a Landmark slot may lower it, by the operator's bounded
        # tolerance and by nothing else.
        self.assertIn("StrategicTier::Landmark", ladder)
        self.assertIn("max_strategic_negative_margin_minor", ladder)

    def test_the_ladder_is_frozen_at_open_and_the_acceptance_is_not(self) -> None:
        # A ladder that moved under a running conversation makes last week's
        # counter unexplainable; a frozen one that nothing re-checks lets a
        # cheap-looking trip talk the agent into a show that no longer clears.
        # Both halves are needed.
        upsert = read(INGRESS).split("ON CONFLICT (workspace_id, opportunity_id) DO UPDATE SET", 1)[1]
        upsert = upsert.split('"#', 1)[0]
        for frozen in ("walk_away_minor", "target_minor", "opening_ask_minor"):
            self.assertNotIn(frozen, upsert, "the ladder is not rewritten on a new offer")
        rule = self.domain.split("pub fn evaluate_terms", 1)[1].split("\n}", 1)[0]
        self.assertIn("clears_now", rule)
        self.assertIn("net_margin(snapshot)", rule)

    def test_the_opening_ask_is_anchored_on_the_floor(self) -> None:
        # Anchoring on the promoter's number lets a deliberately low first offer
        # drag the band's own target down with it.
        ladder = self.domain.split("pub fn terms_ladder", 1)[1].split("\n}", 1)[0]
        self.assertIn("uplift(walk_away_minor, policy.target_uplift_basis_points)", ladder)
        self.assertNotIn("offered_fee_minor", ladder)

    def test_the_asks_are_bounded_and_never_concede_past_the_target(self) -> None:
        ask = self.domain.split("fn counter_ask", 1)[1].split("\n}", 1)[0]
        self.assertIn(".max(terms.ladder.target_minor)", ask)
        self.assertIn("max_counter_rounds", self.domain)
        self.assertIn("counter_rounds integer NOT NULL DEFAULT 0", self.sql)

    # --- what may actually happen ----------------------------------------

    def test_every_outward_move_is_third_party(self) -> None:
        model = read(MODEL)
        classes = model.split("Self::RequestBookingOutreach { .. }", 1)[1].split(
            "ActionClass::ThirdParty", 1
        )[0]
        self.assertIn("Self::CounterLiveOpportunityTerms { .. }", classes)
        self.assertIn("Self::AcceptLiveOpportunityTerms { .. }", classes)
        # And the candidate forces approval on top of the class ceiling.
        candidate = read(CANDIDATE).split("fn live_terms_candidate", 1)[1].split("\n}\n", 1)[0]
        self.assertIn("PolicyDisposition::RequireApproval", candidate)

    def test_a_decline_is_recorded_rather_than_sent(self) -> None:
        # The agent recording that it will not take these terms is a fact an
        # operator can read. Telling the promoter stays a human act, so there is
        # no action for it.
        candidate = read(CANDIDATE).split("fn live_terms_candidate", 1)[1].split("\n}\n", 1)[0]
        self.assertIn("TermsDecision::Decline { .. } | TermsDecision::Expire", candidate)
        loop = read(EVALUATE).split("async fn advance_live_terms", 1)[1].split("\n    }", 1)[0]
        self.assertIn("settle_live_opportunity_terms", loop)
        self.assertIn("terms_settled", loop)
        self.assertIn("TermsState::Declined", loop)
        self.assertIn("TermsState::Expired", loop)

    def test_an_omission_is_written_down(self) -> None:
        # An expiry has no refusal behind it, and leaving the column null would
        # read as a decline whose reason somebody forgot to write.
        settle = self.infra.split("settle_live_opportunity_terms_impl", 1)[1]
        self.assertIn('map_or("window_closed", TermsRefusal::as_str)', settle)
        self.assertIn("settled_at IS NULL", settle)

    def test_the_floor_holds_at_execution_as_well_as_at_decision(self) -> None:
        # Time passes between drafting a move and sending it. Everything else
        # can change harmlessly; accepting below cost cannot.
        executor = read(EXECUTOR).split("async fn execute_live_opportunity_terms", 1)[1]
        executor = executor.split("\n}", 1)[0]
        self.assertIn("if accept && amount_minor < row.4", executor)
        self.assertIn("terms.settled_at IS NULL", executor)
        self.assertIn("terms.responds_by > $3", executor)
        self.assertIn("FOR UPDATE OF terms", executor)
        # A move quoted in another currency is a different offer.
        self.assertIn("if row.5 != currency", executor)

    def test_a_counter_counts_from_the_row_not_from_the_payload(self) -> None:
        # Two executions of the same drafted counter must not count as two asks.
        executor = read(EXECUTOR).split("async fn execute_live_opportunity_terms", 1)[1]
        self.assertIn("counter_rounds=counter_rounds+1", executor)

    def test_one_live_negotiation_per_opportunity_for_ever(self) -> None:
        self.assertIn("UNIQUE (workspace_id, opportunity_id)", self.sql)
        self.assertIn(
            "CHECK ((settled_at IS NOT NULL) = (state IN ('accepted', 'declined', 'expired')))",
            self.sql,
        )
        # A settled negotiation is not reopened by another offer.
        upsert = read(INGRESS).split("ON CONFLICT (workspace_id, opportunity_id) DO UPDATE SET", 1)[1]
        self.assertIn("WHERE viryaos_team_opportunity_terms.settled_at IS NULL", upsert)

    def test_the_send_is_external_work_behind_a_named_capability(self) -> None:
        execution = read(EXECUTION) + "\n" + read(EXECUTION_CAPS)
        self.assertIn(
            'AutopilotActionPayload::CounterLiveOpportunityTerms { .. } => "opportunity.terms"',
            execution,
        )
        self.assertIn('"crowdrelay.opportunity.terms_countered" => "opportunity.terms"', execution)
        self.assertIn('"crowdrelay.opportunity.terms_accepted" => "opportunity.terms"', execution)
        requires = execution.split("fn payload_requires_executor", 1)[1].split("\n}", 1)[0]
        self.assertIn("CounterLiveOpportunityTerms", requires)
        self.assertIn("AcceptLiveOpportunityTerms", requires)

    def test_the_operator_is_the_only_way_a_negotiation_starts(self) -> None:
        openapi = read(OPENAPI)
        self.assertIn("/admin/autopilot/team-opportunities/{opportunity_id}/terms", openapi)
        self.assertIn("recordAutopilotTeamOpportunityTerms", openapi)
        request = openapi.split("    TeamOpportunityTermsRequest:", 1)[1].split(
            "\n    TeamOpportunityProgressRequest:", 1
        )[0]
        self.assertIn("enum: [offer, withdrawn]", request)
        self.assertIn("responds_by", request)
        # Nothing in the evaluation path may create one.
        self.assertNotIn("INSERT INTO viryaos_team_opportunity_terms", self.infra)

    def test_both_halves_of_the_pipeline_are_costed_the_same_way(self) -> None:
        # Two ways of costing one trip is how a negotiation floor and an
        # economics verdict come to disagree about the same show.
        self.assertIn("load_live_opportunity_snapshots_for", self.infra)
        self.assertIn('&["submitted", "replied"]', self.infra)
        self.assertIn("load_live_opportunity_snapshots_for", read(INGRESS))


if __name__ == "__main__":
    unittest.main()
