"""The safety pass of the growth-plane audit — phase 17.

Every other contract test guards one feature. This one guards the claims the
whole autonomous plane rests on, in one place, so that adding a context cannot
quietly cost one of them:

- every candidate passes through one funnel, and the class ceiling is applied
  there rather than in twenty detectors;
- the volume envelope is applied after the ceiling, never instead of it;
- the per-contact cooldown sees contacts made earlier in the same cycle;
- an absent authority row reads as the safest ceiling, never as no limit;
- a record may narrow what the agent does and may never widen it;
- nothing writes an external metric point from our own actions;
- a gig whose economics cannot be computed is never treated as profitable;
- the kill switch exists and is read from configuration.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
EVALUATE = ROOT / "crates/crowdrelay-application/src/autopilot/evaluate.rs"
PORTS = ROOT / "crates/crowdrelay-application/src/autopilot/ports.rs"
ENVELOPE = ROOT / "crates/crowdrelay-domain/src/growth_envelope.rs"
ACTION_CLASS = ROOT / "crates/crowdrelay-domain/src/action_class.rs"
LEARNING = ROOT / "crates/crowdrelay-domain/src/learning.rs"
TOUR = ROOT / "crates/crowdrelay-domain/src/tour_economics.rs"
CONFIG = ROOT / "crates/crowdrelay-infra/src/config.rs"
APPLICATION = ROOT / "crates/crowdrelay-application/src"
INFRA = ROOT / "crates/crowdrelay-infra/src"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


class GrowthSafetyAudit(unittest.TestCase):
    def setUp(self) -> None:
        self.evaluate = read(EVALUATE)

    def test_every_candidate_passes_through_one_funnel(self) -> None:
        # Twenty detectors each remembering to clamp is twenty chances to
        # forget. `persist` is the only caller of the write.
        callers = [
            path
            for path in APPLICATION.rglob("*.rs")
            if "persist_candidate(" in read(path) and path.name != "ports.rs"
        ]
        self.assertEqual([path.name for path in callers], ["evaluate.rs"])
        self.assertEqual(self.evaluate.count(".persist_candidate("), 1)
        funnel = self.evaluate.split("async fn persist(", 1)[1]
        self.assertIn("clamp_disposition(candidate.disposition, ceiling)", funnel)

    def test_an_absent_authority_row_is_the_safest_ceiling(self) -> None:
        funnel = self.evaluate.split("async fn persist(", 1)[1]
        self.assertIn("unwrap_or_else(|| class.safest_ceiling())", funnel)
        # And the safest ceiling for anything outward is a human.
        safest = read(ACTION_CLASS).split("pub const fn safest_ceiling", 1)[1].split(
            "\n    }", 1
        )[0]
        self.assertIn("ThirdParty | Self::Paid => AutonomyLevel::RequireApproval", safest)

    def test_the_envelope_applies_after_the_ceiling_and_never_instead_of_it(self) -> None:
        funnel = self.evaluate.split("async fn persist(", 1)[1]
        ceiling = funnel.index("clamp_disposition(candidate.disposition, ceiling)")
        envelope = funnel.index("check_envelope(")
        self.assertLess(ceiling, envelope)
        # The envelope may only ever lower the disposition further.
        held = funnel.split("EnvelopeVerdict::Hold(block)", 1)[1].split("};", 1)[0]
        self.assertIn("AutonomyLevel::RequireApproval", held)
        self.assertIn("AutonomyLevel::Recommend", held)
        self.assertNotIn("BoundedAuto", held)

    def test_the_cooldown_sees_contacts_made_earlier_in_the_same_cycle(self) -> None:
        # The touch ages are a snapshot taken before the cycle. Without this,
        # two contexts can each pass the cooldown against the same stale
        # reading and between them message one person twice in a minute.
        funnel = self.evaluate.split("async fn persist(", 1)[1]
        self.assertIn(".contains(&candidate.subject.uuid())", funnel)
        self.assertIn("touched_this_cycle", funnel)
        self.assertIn("return Some(0);", funnel)
        self.assertIn("is_contactable_person()", funnel)

    def test_the_spend_is_topped_up_inside_the_cycle(self) -> None:
        # A cap read once at the start lets a single cycle with fifty findings
        # enqueue all fifty against a budget of five.
        funnel = self.evaluate.split("async fn persist(", 1)[1]
        self.assertIn("owned_audience_touches_7d.saturating_add(1)", funnel)
        self.assertIn("third_party_touches_7d.saturating_add(1)", funnel)

    def test_a_record_may_narrow_and_never_widen(self) -> None:
        learning = read(LEARNING)
        # Nothing in the learning module can reach authority.
        for authority in ("ActionClass", "AutonomyLevel", "GrowthEnvelope", "clamp_disposition"):
            self.assertNotIn(authority, learning)
        self.assertIn("a_perfect_record_never_widens_anything", learning)
        self.assertIn("a_configured_zero_is_never_raised_to_one", learning)

    def test_nothing_fabricates_an_external_observation(self) -> None:
        # An external metric point may only come from an adapter reporting what
        # a platform said. A point written from our own action would be the
        # agent marking its own homework.
        writers = [
            path
            for path in INFRA.rglob("*.rs")
            if "INSERT INTO viryaos_growth_metric_points" in read(path)
        ]
        self.assertEqual([path.name for path in writers], ["growth_metrics.rs"])

    def test_an_uncomputable_gig_is_never_profitable(self) -> None:
        clears = read(TOUR).split("pub const fn clears_floor", 1)[1].split("\n    }", 1)[0]
        self.assertIn("Self::Insufficient { .. } => false", clears)

    def test_the_kill_switch_exists_and_is_configuration(self) -> None:
        self.assertIn("CROWDRELAY_AUTOPILOT_ENABLED", read(CONFIG))

    def test_the_envelope_names_every_block_it_can_apply(self) -> None:
        # A hold with no name is a hold nobody can explain to an operator.
        envelope = read(ENVELOPE)
        self.assertIn("may_offer_for_approval", envelope)
        block = envelope.split("pub const fn as_str", 1)[1].split("\n    }", 1)[0]
        blocks = set(re.findall(r'Self::\w+(?: \{ \.\. \})? => "([a-z_]+)"', block))
        # Every reason the envelope can refuse for has a name an operator reads.
        self.assertEqual(
            blocks,
            {
                "agent_disabled",
                "dry_run",
                "weekly_budget_exhausted",
                "subject_in_cooldown",
            },
        )


if __name__ == "__main__":
    unittest.main()
