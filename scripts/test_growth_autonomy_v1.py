"""Contract tests for the autonomy ceiling.

This is the mechanism that makes "the agent did that on its own" a sentence an
operator can hear calmly, so the properties pinned here are the ones whose
failure is silent and expensive: a ceiling that stops applying, a ceiling that
promotes instead of demotes, a missing row read as unlimited authority, and a
new action kind that slips in unclassified.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
MIGRATION = ROOT / "migrations/0075_viryaos_growth_autonomy.sql"
DOMAIN = ROOT / "crates/crowdrelay-domain/src/action_class.rs"
MODEL = ROOT / "crates/crowdrelay-application/src/autopilot/model.rs"
EVALUATE = ROOT / "crates/crowdrelay-application/src/autopilot/evaluate.rs"
LOADER = ROOT / "crates/crowdrelay-infra/src/autopilot/decisions/opportunity_reads.rs"

CLASSES = ("first_party_reversible", "owned_audience", "third_party", "paid")


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


class GrowthAutonomyContract(unittest.TestCase):
    def setUp(self) -> None:
        self.migration = read(MIGRATION)
        self.domain = read(DOMAIN)
        self.model = read(MODEL)

    def test_money_and_third_party_contact_are_seeded_gated(self) -> None:
        # The operator chose safest real autonomy. If this ever seeds
        # 'bounded_auto', a fresh workspace can cold-approach a curator on its
        # first cycle.
        for gated in ("third_party", "paid"):
            for statement in re.findall(
                rf"\('{gated}', '(\w+)'", self.migration
            ):
                self.assertEqual(statement, "require_approval")

    def test_the_agent_may_act_alone_only_on_its_own_surfaces_and_audience(self) -> None:
        for allowed in ("first_party_reversible", "owned_audience"):
            levels = set(re.findall(rf"\('{allowed}', '(\w+)'", self.migration))
            self.assertEqual(levels, {"bounded_auto"})

    def test_every_class_is_seeded_for_existing_and_future_workspaces(self) -> None:
        backfill = self.migration.split("CROSS JOIN (VALUES", 1)[1].split(") AS seed", 1)[0]
        provisioning = self.migration.split(
            "CREATE OR REPLACE FUNCTION viryaos_provision_growth_autonomy", 1
        )[1]
        for action_class in CLASSES:
            self.assertIn(f"'{action_class}'", backfill)
            self.assertIn(f"'{action_class}'", provisioning)

    def test_the_database_classes_match_the_rust_enum(self) -> None:
        block = self.domain.split("impl ActionClass", 1)[1].split("pub fn parse", 1)[0]
        rust = set(re.findall(r'Self::\w+ => "([a-z_]+)"', block))
        constraint = self.domain and self.migration.split(
            "action_class text NOT NULL CHECK (action_class IN (", 1
        )[1].split("))", 1)[0]
        self.assertEqual(rust, set(re.findall(r"'([a-z_]+)'", constraint)))
        self.assertEqual(rust, set(CLASSES))

    def test_a_missing_ceiling_row_is_never_read_as_unlimited_authority(self) -> None:
        # A migration that has not run must not be a grant of authority.
        persist = self.evaluate_persist()
        self.assertIn("unwrap_or_else(|| class.safest_ceiling())", persist)
        self.assertIn("pub const fn safest_ceiling", self.domain)

    def test_an_unreadable_ceiling_row_falls_back_instead_of_guessing(self) -> None:
        loader = read(LOADER).split("fn load_autonomy_ceilings_impl", 1)[1].split(
            "\n    async fn", 1
        )[0]
        # filter_map drops the row, and the caller then applies safest_ceiling.
        self.assertIn("filter_map", loader)
        self.assertIn("ActionClass::parse(&class)?", loader)

    def evaluate_persist(self) -> str:
        return read(EVALUATE).split("async fn persist(", 1)[1].split("\n    }", 1)[0]

    def test_the_ceiling_is_applied_at_one_choke_point_every_candidate_passes(
        self,
    ) -> None:
        # Applying it inside each candidate function would mean a new detector
        # could forget it, or its author could choose to skip it.
        persist = self.evaluate_persist()
        self.assertIn("candidate.action.action_class()", persist)
        self.assertIn("clamp_disposition", persist)
        # Not "appears once" — the envelope downgrades through the same helper.
        # What must hold is that no other function touches authority at all.
        evaluate = read(EVALUATE)
        self.assertEqual(
            evaluate.count("clamp_disposition("), persist.count("clamp_disposition(")
        )
        # Every dispatch arm reaches the database through this one function.
        self.assertNotIn("persist_candidate(self.workspace_id", evaluate.split("async fn persist(", 1)[0])

    def test_the_ceiling_only_ever_lowers_authority(self) -> None:
        self.assertIn(
            "a_ceiling_lowers_an_auto_execute_decision_but_never_raises_one", self.domain
        )
        self.assertIn("a_denied_decision_is_never_reopened_by_a_ceiling", self.domain)

    def test_every_action_payload_is_classified_exhaustively(self) -> None:
        # A lookup table keyed by action_kind would silently default a new
        # action to whatever the fallback was. A match cannot.
        block = self.model.split("pub const fn action_class(&self)", 1)[1].split(
            "pub const fn action_kind", 1
        )[0]
        self.assertNotIn("_ =>", block)
        kinds = self.model.split("pub const fn action_kind", 1)[1]
        variants = set(re.findall(r"Self::(\w+)", kinds.split("\n    }", 1)[0]))
        classified = set(re.findall(r"Self::(\w+)", block))
        self.assertEqual(
            variants - classified,
            set(),
            "every action payload must declare what it costs and who it reaches",
        )

    def test_reach_is_decided_per_lever_and_per_milestone_not_per_variant(self) -> None:
        # One class for the whole variant would be wrong in both directions: it
        # would gate a push to our own fans, or let a press approach go out
        # unattended.
        block = self.model.split("pub const fn action_class(&self)", 1)[1].split(
            "pub const fn action_kind", 1
        )[0]
        self.assertIn("ShowGrowthLever::PartnerCrossPromo", block)
        self.assertIn("ReleaseMilestone::StartPress", block)
        press = block.split("ReleaseMilestone::StartPress => ", 1)[1].split(",", 1)[0]
        self.assertEqual(press, "ActionClass::ThirdParty")

    def test_outreach_actions_can_never_be_classified_as_our_own_audience(self) -> None:
        block = self.model.split("pub const fn action_class(&self)", 1)[1].split(
            "pub const fn action_kind", 1
        )[0]
        third_party = block.split("=> ActionClass::ThirdParty", 1)[0].rsplit(
            "ActionClass::Paid,", 1
        )[1]
        for required in (
            "RequestBookingOutreach",
            "RequestOutreach",
            "RequestBeaconOutreach",
            "ApplyLiveOpportunity",
            "SubmitFundingApplication",
        ):
            self.assertIn(required, third_party)

    def test_gated_decisions_are_counted_apart_from_throttled_ones(self) -> None:
        # Throttled work is deferred; gated work is somebody's decision to make.
        report = read(EVALUATE).split("pub struct AutopilotCycleReport {", 1)[1].split(
            "}", 1
        )[0]
        self.assertIn("actions_gated", report)
        self.assertIn("actions_throttled", report)


if __name__ == "__main__":
    unittest.main()
