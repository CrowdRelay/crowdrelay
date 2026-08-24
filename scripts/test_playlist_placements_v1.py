"""Contract tests for Phase 12's anti-scam core — verified placements.

A pitcher that counts sends is a spam cannon with a dashboard. The number that
matters is placements, and placements are exactly the number somebody has a
motive to lie about: the playlist-promotion economy runs on screenshots of adds
that were removed the following week.

Every claim pinned here is one that, if it stopped holding, would turn the
measured record into somebody else's marketing:

- a claim nobody confirmed is never counted;
- a read that failed is not a read that found nothing;
- a confirmation that disappears suppresses the operator, not the playlist;
- and the second read of a placement is not deduplicated against the first,
  which is precisely the check a scammer needs to miss.

Screening is not tested here. It landed with Phase 9 and `RefusalReason`
already carries the paid-placement, bought-audience and churn cases.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
MIGRATION = ROOT / "migrations/0099_viryaos_playlist_placements.sql"
DOMAIN = ROOT / "crates/crowdrelay-domain/src/playlist_placement.rs"
SCREENING = ROOT / "crates/crowdrelay-domain/src/target_discovery.rs"
MODEL = ROOT / "crates/crowdrelay-application/src/autopilot/model.rs"
CANDIDATE = ROOT / "crates/crowdrelay-application/src/autopilot/evaluate/placements.rs"
EVALUATE = ROOT / "crates/crowdrelay-application/src/autopilot/evaluate.rs"
INFRA = ROOT / "crates/crowdrelay-infra/src/autopilot/placements.rs"
EXECUTION = ROOT / "crates/crowdrelay-infra/src/autopilot/execution.rs"
OPENAPI = ROOT / "openapi/openapi.yaml"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def strip_sql_comments(sql: str) -> str:
    return "\n".join(line.split("--", 1)[0] for line in sql.splitlines())


class PlaylistPlacementContract(unittest.TestCase):
    def setUp(self) -> None:
        self.sql = strip_sql_comments(read(MIGRATION))
        self.domain = read(DOMAIN)
        self.infra = read(INFRA)

    # --- a claim is not a placement --------------------------------------

    def test_only_a_verified_placement_may_be_counted(self) -> None:
        # A placement that cannot be verified never reaches a result, or the
        # learning layer is trained on somebody else's marketing.
        countable = self.domain.split("pub const fn countable", 1)[1].split("\n    }", 1)[0]
        self.assertIn("matches!(self, Self::Verified)", countable)

    def test_a_claim_nobody_confirms_is_ghosted_rather_than_counted(self) -> None:
        rule = self.domain.split("pub fn evaluate_placement", 1)[1].split("\n}", 1)[0]
        self.assertIn("confirm_within_hours", rule)
        self.assertIn("PlacementState::Ghosted", rule)

    def test_an_unreadable_read_settles_nothing_and_consumes_no_checkpoint(self) -> None:
        # A dead credential is not evidence that a track is gone. Counting it
        # would mark honest curators as scammers and burn the schedule.
        apply = self.domain.split("pub fn apply_observation", 1)[1].split("\n}", 1)[0]
        self.assertIn("PlacementObservation::Unreadable => (snapshot.state, false)", apply)
        update = self.infra.split("record_playlist_placement_operator", 1)[1]
        update = update.split("checks_completed = LEAST", 1)[1].split('"#', 1)[0]
        self.assertIn("checks_completed + CASE WHEN $4 THEN 1 ELSE 0 END", 
                      "checks_completed = LEAST" + update)
        # `state` and `settled_at` move together or the schema refuses the row,
        # so a settling read is handed to the settlement path rather than
        # written here — which is also what carries the suppression.
        self.assertIn("WHERE workspace_id = $1 AND opportunity_id = $2 AND settled_at IS NULL",
                      update)
        operator = self.infra.split("record_playlist_placement_operator", 1)[1]
        self.assertIn("SET state = CASE WHEN $7 THEN state ELSE $3 END", operator)
        self.assertIn("if state.settled()", operator)
        self.assertIn("settle_playlist_placement_impl", operator)

    def test_an_absence_before_a_confirmation_is_ghosted_not_withdrawn(self) -> None:
        # The two predict differently next release, so collapsing them loses a
        # distinction the ranker needs.
        apply = self.domain.split("pub fn apply_observation", 1)[1].split("\n}", 1)[0]
        absent = apply.split("PlacementObservation::Absent =>", 1)[1]
        self.assertIn("PlacementState::Verified => (PlacementState::Withdrawn", absent)
        self.assertIn("PlacementState::Claimed => (PlacementState::Ghosted", absent)

    # --- verification repeats, and cannot be short-circuited -------------

    def test_the_recheck_is_a_separate_decision_from_the_first_read(self) -> None:
        # Keyed without the checkpoint, the second read is deduplicated against
        # the first and never happens — the exact check a scammer needs to miss.
        candidate = read(CANDIDATE)
        keys = re.findall(r'(?:decision_key|action_idempotency_key): format!\(\s*"([^"]+)"', candidate)
        self.assertEqual(len(keys), 2)
        for key in keys:
            self.assertEqual(key.count("{}"), key.count("{}"))
        self.assertIn("checkpoint\n        ),", candidate)

    def test_the_schedule_is_not_an_operator_setting(self) -> None:
        # A re-check window somebody could widen is a window a scammer waits out.
        loop = read(EVALUATE).split("async fn follow_through_placements", 1)[1].split("\n    }", 1)[0]
        self.assertIn("PlacementPolicy::default()", loop)
        self.assertNotIn("AutopilotPolicyConfig::", loop)

    def test_three_real_reads_close_the_window(self) -> None:
        rule = self.domain.split("pub fn evaluate_placement", 1)[1].split("\n}", 1)[0]
        self.assertIn("snapshot.checks_completed >= 3", rule)
        self.assertIn("checks_completed smallint NOT NULL DEFAULT 0", self.sql)
        self.assertIn("CHECK (checks_completed BETWEEN 0 AND 3)", self.sql)

    def test_a_withdrawal_settles_the_moment_it_is_seen(self) -> None:
        # Waiting for the last checkpoint leaves a curator we already know
        # about being pitched in the meantime.
        rule = self.domain.split("pub fn evaluate_placement", 1)[1].split("\n}", 1)[0]
        withdrawn = rule.index("PlacementState::Withdrawn")
        ghosted = rule.index("PlacementState::Ghosted")
        self.assertLess(withdrawn, ghosted)

    # --- suppression follows the operator --------------------------------

    def test_a_withdrawal_suppresses_the_curator_not_the_playlist(self) -> None:
        # One person runs dozens of lists. Suppressing the one they pulled the
        # track from is how the same curator is pitched next week under another
        # name.
        self.assertIn("pub const fn suppresses_identity", self.domain)
        settle = self.infra.split("settle_playlist_placement_impl", 1)[1]
        self.assertIn("suppresses_identity(settlement.state)", settle)
        self.assertIn("curator_identity", settle)
        self.assertIn("do_not_contact = true", settle)
        # And their open opportunities go with them, or the next cycle pitches
        # a suppressed curator.
        self.assertIn("SET active = false", settle)

    def test_the_suppression_and_the_settlement_are_one_transaction(self) -> None:
        settle = self.infra.split("settle_playlist_placement_impl", 1)[1].split("\n    }", 1)[0]
        self.assertIn("self.pool.begin()", settle)
        self.assertIn("transaction.commit()", settle)
        self.assertEqual(settle.count("self.pool.begin()"), 1)

    def test_an_unknown_identity_suppresses_only_its_own_target(self) -> None:
        # Matching NULL to NULL would suppress every target the workspace has
        # never identified.
        settle = self.infra.split("settle_playlist_placement_impl", 1)[1]
        self.assertIn("target.curator_identity IS NOT NULL", settle)
        self.assertIn("target.id = $2", settle)
        self.assertIn("curator_identity IS NULL OR", self.sql)

    def test_verified_is_not_a_settled_state(self) -> None:
        # Writing it as settled would stop the re-checks that make it mean
        # anything.
        settled = self.domain.split("pub const fn settled", 1)[1].split("\n    }", 1)[0]
        self.assertIn("Self::Ghosted | Self::Withdrawn", settled)
        self.assertNotIn("Verified", settled)
        self.assertIn("if !settlement.state.settled()", self.infra)

    # --- the closed sets stay closed --------------------------------------

    def test_the_stored_states_and_observations_match_the_rust_enums(self) -> None:
        for pattern, header in (
            (r"state IN \((.*?)\)", "impl PlacementState"),
            (r"last_observation IN \((.*?)\)", "impl PlacementObservation"),
        ):
            stored = re.search(pattern, self.sql, re.DOTALL)
            self.assertIsNotNone(stored, pattern)
            declared = set(
                re.findall(
                    r'Self::\w+ => "([a-z_]+)"',
                    self.domain.split(header, 1)[1].split("pub fn parse", 1)[0],
                )
            )
            stored = set(re.findall(r"'([a-z_]+)'", stored.group(1)))
            if header == "impl PlacementObservation":
                # `unreadable` is never stored: it is not a read, so there is
                # nothing to record about it.
                self.assertEqual(declared - stored, {"unreadable"})
            else:
                self.assertEqual(stored, declared)

    def test_one_placement_per_pitch(self) -> None:
        self.assertIn("UNIQUE (workspace_id, opportunity_id)", self.sql)
        self.assertIn(
            "CHECK ((settled_at IS NOT NULL) = (state IN ('ghosted', 'withdrawn')))", self.sql
        )
        self.assertIn("CHECK ((last_observation IS NULL) = (last_checked_at IS NULL))", self.sql)

    def test_the_read_is_first_party_and_behind_a_named_capability(self) -> None:
        # It contacts nobody, which is exactly why it may run unattended: the
        # point is checking a claim without asking the person who made it.
        model = read(MODEL)
        classes = model.split("Self::SendTeamAssignmentEmail { .. }", 1)[1].split(
            "ActionClass::FirstPartyReversible", 1
        )[0]
        self.assertIn("Self::VerifyPlaylistPlacement { .. }", classes)
        execution = read(EXECUTION)
        self.assertIn(
            'AutopilotActionPayload::VerifyPlaylistPlacement { .. } => "playlist.verify"',
            execution,
        )
        self.assertIn(
            '"viryaos.playlist.placement_check_requested" => "playlist.verify"', execution
        )

    def test_the_screening_rules_are_the_ones_phase_nine_already_built(self) -> None:
        # Rebuilding them here would leave two lists of refusals that drift.
        refusals = read(SCREENING).split("impl RefusalReason", 1)[1].split("\n}", 1)[0]
        for reason in ("paid_placement", "sells_placement", "implausible_engagement",
                       "indiscriminate_churn"):
            self.assertIn(f'"{reason}"', refusals)
        self.assertNotIn("RefusalReason", self.domain)

    def test_a_placement_only_enters_through_the_operator_ingress(self) -> None:
        openapi = read(OPENAPI)
        self.assertIn("/admin/autopilot/playlist-placements", openapi)
        self.assertIn("recordAutopilotPlaylistPlacement", openapi)
        request = openapi.split("    PlaylistPlacementRequest:", 1)[1].split(
            "\n    TeamOpportunityTermsRequest:", 1
        )[0]
        self.assertIn("enum: [claimed, present, absent, unreadable]", request)
        # Nothing in the cycle may invent one.
        self.assertNotIn("INSERT INTO viryaos_playlist_placements", read(EVALUATE))


if __name__ == "__main__":
    unittest.main()
