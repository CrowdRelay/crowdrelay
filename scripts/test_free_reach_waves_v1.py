"""Contract tests for Phase 10 — free-reach pitching, presented as waves.

The outreach context could already pitch one target at a time. Nothing about
relevance, cadence, follow-ups or decline cooldowns changes here, and the tests
that would catch it changing are the ones already in `test_growth_envelope_v1`
and the outreach contract.

What this file pins is the part that is new, and it is a claim about people
rather than about code: forty individual approvals is how a human stops
approving. So the properties worth a test are the ones that keep a wave a thing
somebody can actually say yes to — it is sized to a budget that exists, it stops
growing once somebody is reading it, and it takes its unapproved pitches with it
when the moment passes.

The one that would cost real money if it broke: a wave must never widen the
envelope. It is presentation, not permission.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
MIGRATION = ROOT / "migrations/0098_viryaos_outreach_waves.sql"
DOMAIN = ROOT / "crates/crowdrelay-domain/src/free_reach.rs"
OUTREACH = ROOT / "crates/crowdrelay-domain/src/outreach.rs"
MODEL = ROOT / "crates/crowdrelay-application/src/autopilot/model.rs"
CANDIDATE = ROOT / "crates/crowdrelay-application/src/autopilot/evaluate/candidates.rs"
EVALUATE = ROOT / "crates/crowdrelay-application/src/autopilot/evaluate.rs"
WAVES = ROOT / "crates/crowdrelay-application/src/autopilot/evaluate/outreach_supply.rs"
INFRA = ROOT / "crates/crowdrelay-infra/src/autopilot/waves.rs"
ACTIONS = ROOT / "crates/crowdrelay-infra/src/autopilot/actions_execution.rs"
OPENAPI = ROOT / "openapi/openapi.yaml"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def strip_sql_comments(sql: str) -> str:
    return "\n".join(line.split("--", 1)[0] for line in sql.splitlines())


class FreeReachWavesContract(unittest.TestCase):
    def setUp(self) -> None:
        self.sql = strip_sql_comments(read(MIGRATION))
        self.domain = read(DOMAIN)
        self.infra = read(INFRA)

    # --- a wave is presentation, never permission ------------------------

    def test_a_wave_is_never_bigger_than_the_budget_that_exists(self) -> None:
        # The one claim here that costs real money if it breaks. Sized above the
        # remaining weekly budget, the wave is throttled halfway through and the
        # operator has approved forty pitches to watch six go out.
        capacity = self.domain.split("pub fn wave_capacity", 1)[1].split("\n}", 1)[0]
        self.assertIn("third_party_budget_remaining", capacity)
        self.assertIn("policy.max_pitches_per_wave.min(budget)", capacity)
        # And the budget read is the same envelope and usage the cycle already
        # throttles against, not a second opinion about the same number.
        opening = read(WAVES).split("pub(super) async fn open_outreach_waves", 1)[1].split(
            "\n    }", 1
        )[0]
        self.assertIn("load_growth_envelope", opening)
        self.assertIn("weekly_third_party_touches", opening)
        self.assertIn("usage.third_party_touches_7d", opening)

    def test_a_wave_pitch_is_the_same_pitch(self) -> None:
        # Same cadence, same relevance bar, same idempotency key. Keying it
        # identically is what makes the wave path and the standing path safe to
        # run side by side: the second insert is deduplicated by the database
        # rather than by a rule somebody has to remember.
        candidate = read(CANDIDATE).split("fn outreach_candidate", 1)[1].split("\n}\n", 1)[0]
        self.assertIn("wave_id", candidate)
        keys = re.findall(r'action_idempotency_key: format!\(\s*"([^"]+)"', candidate)
        self.assertEqual(len(keys), 1)
        self.assertNotIn("wave", keys[0], "a wave must not change a pitch's identity")

    def test_a_pitch_inside_a_wave_is_never_executed_on_its_own(self) -> None:
        candidate = read(CANDIDATE).split("fn outreach_candidate", 1)[1].split("\n}\n", 1)[0]
        self.assertIn("wave_id.is_some()", candidate)
        self.assertIn("PolicyDisposition::RequireApproval", candidate)

    def test_membership_lives_in_the_payload_not_on_the_actions_table(self) -> None:
        # A column for one context's concern on the hottest table in the system.
        self.assertNotIn("ALTER TABLE viryaos_autopilot_actions ADD COLUMN", self.sql)
        self.assertIn("payload->>'wave_id'", self.infra)
        self.assertIn("viryaos_autopilot_actions_wave_idx", self.sql)

    # --- and it is a thing somebody can say yes to -----------------------

    def test_only_a_sealed_wave_may_be_approved(self) -> None:
        # A wave still drafting would grow after it was read.
        approve = self.infra.split("async fn approve_outreach_wave_impl", 1)[1]
        self.assertIn("state='sealed'", approve)
        self.assertIn("RepositoryError::Conflict", approve)

    def test_the_whole_wave_is_approved_in_one_statement(self) -> None:
        # Pitch by pitch inside a loop, an error halfway leaves a half-approved
        # batch — the one state an operator cannot reason about, because the
        # thing they approved was the batch.
        approve = self.infra.split("async fn approve_outreach_wave_impl", 1)[1]
        updates = re.findall(r"UPDATE viryaos_autopilot_actions", approve)
        self.assertEqual(len(updates), 1)
        self.assertIn("status='awaiting_approval'", approve)
        self.assertIn("approval_expires_at IS NULL OR approval_expires_at > $3", approve)

    def test_the_agent_leaves_a_sealed_wave_alone(self) -> None:
        rule = self.domain.split("pub fn evaluate_wave", 1)[1].split("\n}", 1)[0]
        self.assertIn("WaveState::Sealed", rule)
        self.assertIn("WaveHold::NotOurs", rule)

    def test_an_expiring_wave_takes_its_unapproved_pitches_with_it(self) -> None:
        # Leaving them queued sends a release-week pitch a month late, one at a
        # time, with nobody having decided to.
        transition = self.infra.split("async fn transition_outreach_wave_impl", 1)[1]
        expire = transition.split("OutreachWaveTransition::Expire", 1)[1]
        self.assertIn("status='cancelled'", expire)
        self.assertIn("status = 'awaiting_approval'", expire)
        self.assertIn("state='expired'", expire)

    def test_a_wave_too_small_to_read_expires_rather_than_seals(self) -> None:
        # One pitch presented as a wave trains an operator to click approve
        # without reading, which is the failure waves exist to prevent.
        rule = self.domain.split("pub fn evaluate_wave", 1)[1].split("\n}", 1)[0]
        self.assertIn("min_pitches_per_wave", rule)
        self.assertIn("WaveExpiry::TooFewPitches", rule)

    def test_a_passed_or_withdrawn_anchor_beats_everything(self) -> None:
        rule = self.domain.split("pub fn evaluate_wave", 1)[1].split("\n}", 1)[0]
        withdrawn = rule.index("WaveExpiry::AnchorWithdrawn")
        passed = rule.index("WaveExpiry::AnchorPassed")
        adds = rule.index("WaveDecision::AddPitch")
        self.assertLess(withdrawn, passed)
        self.assertLess(passed, adds)

    def test_one_wave_per_kind_per_anchor_for_ever(self) -> None:
        self.assertIn("UNIQUE (workspace_id, anchor_kind, anchor_id, target_kind)", self.sql)
        self.assertIn(
            "CHECK ((settled_at IS NOT NULL) = (state IN ('approved', 'expired')))", self.sql
        )
        self.assertIn("ON CONFLICT (workspace_id, anchor_kind, anchor_id, target_kind) DO NOTHING",
                      self.infra)

    def test_the_stored_states_and_reasons_match_the_rust_enums(self) -> None:
        for pattern, header in (
            (r"state IN \((.*?)\)", "impl WaveState"),
            (r"expiry_reason IN \((.*?)\)", "impl WaveExpiry"),
        ):
            stored = re.search(pattern, self.sql, re.DOTALL)
            self.assertIsNotNone(stored, pattern)
            declared = set(
                re.findall(
                    r'Self::\w+ => "([a-z_]+)"',
                    self.domain.split(header, 1)[1].split("\n}", 1)[0],
                )
            )
            self.assertEqual(set(re.findall(r"'([a-z_]+)'", stored.group(1))), declared)

    # --- what a pitch is allowed to claim --------------------------------

    def test_the_evidence_packet_omits_what_it_cannot_read(self) -> None:
        # A zero the agent invented reads exactly like a zero it measured, and
        # the difference is the whole reason this exists.
        packet = read(MODEL).split("pub struct EvidencePacket", 1)[1].split("\n}", 1)[0]
        for field in (
            "trackers",
            "paid_tickets_90d",
            "shows_played_12m",
            "positive_replies_12m",
            "as_of",
        ):
            self.assertIn(f"pub {field}: Option<", packet)

    def test_the_numbers_are_read_when_the_pitch_is_sent(self) -> None:
        # What goes out is what was true when the band said it, not when the
        # agent drafted it.
        emit = read(ACTIONS).split("AutopilotActionPayload::RequestOutreach {", 1)[1]
        emit = emit.split("viryaos.outreach.requested", 1)[1]
        self.assertIn('"evidence": evidence', emit)
        assembly = self.infra.split("pub(super) async fn evidence_packet", 1)[1]
        self.assertIn("ticket_orders", assembly)
        self.assertIn("events", assembly)
        self.assertIn("last_reply_disposition = 'positive'", assembly)
        self.assertIn("as_of: Some(now)", assembly)

    def test_the_wave_knobs_live_with_the_outreach_policy(self) -> None:
        # Same operator setting: how this workspace approaches people it does
        # not know, and how much of that a human is asked to read at once.
        outreach = read(OUTREACH)
        self.assertIn("pub waves: FreeReachPolicy", outreach)
        # And a stored config that predates them must still parse, or the whole
        # context goes down for one missing field.
        policy = outreach.split("pub struct OutreachPolicy", 1)[0]
        self.assertIn("#[serde(default)]", policy.rsplit("#[derive", 1)[1])

    def test_the_operator_approves_a_wave_in_one_call(self) -> None:
        openapi = read(OPENAPI)
        self.assertIn("/admin/autopilot/outreach-waves/{wave_id}/approve", openapi)
        self.assertIn("approveAutopilotOutreachWave", openapi)


if __name__ == "__main__":
    unittest.main()
