"""Contract tests for the Spotify editorial pitch — the one thing the agent must never claim.

The pitch is a single form per release inside Spotify for Artists and there is
no API for it. Everything around the form is the agent's: detect the release,
work out the deadline the distributor's delivery imposes, assemble the text and
the evidence, park it for a human, and refuse to let the deadline slip quietly.

Pressing submit is not, and the failure mode this file exists to prevent is an
agent that reports the pitch as sent. That is worse than not pitching, because a
release then goes out with no editorial consideration and a green dashboard.

So: only a human can mark it done, the chase repeats until they do, and the
window closes at the release rather than staying open for ever.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
MIGRATION = ROOT / "migrations/0100_viryaos_release_editorial_pitch.sql"
DOMAIN = ROOT / "crates/crowdrelay-domain/src/release_autopilot.rs"
MODEL = ROOT / "crates/crowdrelay-application/src/autopilot/model.rs"
CANDIDATE = ROOT / "crates/crowdrelay-application/src/autopilot/evaluate/candidates.rs"
EXECUTOR = ROOT / "crates/crowdrelay-infra/src/autopilot/operations/execution.rs"
INGRESS = ROOT / "crates/crowdrelay-infra/src/autopilot/operations/ingress/team.rs"
OPENAPI = ROOT / "openapi/openapi.yaml"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def strip_sql_comments(sql: str) -> str:
    return "\n".join(line.split("--", 1)[0] for line in sql.splitlines())


class EditorialPitchContract(unittest.TestCase):
    def setUp(self) -> None:
        self.sql = strip_sql_comments(read(MIGRATION))
        self.domain = read(DOMAIN)
        self.executor = read(EXECUTOR)

    def test_the_agent_never_reports_the_form_as_submitted(self) -> None:
        # The whole point. An agent that claims this is an agent whose dashboard
        # is worse than no dashboard.
        for emitted in ("editorial_pitch_parked", "editorial_pitch_escalated"):
            block = self.executor if emitted == "editorial_pitch_parked" else self.executor
            self.assertIn(f'"viryaos.release.{emitted}"', block)
        self.assertEqual(
            self.executor.count('"submitted_by_agent": false'),
            2,
            "both the parking and the chase say plainly that nobody submitted anything",
        )

    def test_only_a_human_can_mark_it_done(self) -> None:
        ingress = read(INGRESS).split("async fn complete_editorial_pitch", 1)[1]
        self.assertIn("editorial_pitch_completed_at=now()", ingress)
        # Guarded, so a second click is not a second submission.
        self.assertIn("editorial_pitch_completed_at IS NULL", ingress)
        # And nothing in the cycle writes it.
        self.assertNotIn("editorial_pitch_completed_at=", read(CANDIDATE))
        escalate = self.executor.split("async fn escalate_editorial_pitch", 1)[1]
        self.assertNotIn("editorial_pitch_completed_at=", escalate.split("\n}", 1)[0])

    def test_parking_reaches_nobody_outside_the_workspace(self) -> None:
        model = read(MODEL)
        classes = model.split("Self::ExecuteReleaseMilestone { milestone, .. }", 1)[1]
        first_party = classes.split("ActionClass::FirstPartyReversible", 1)[0]
        self.assertIn("ReleaseMilestone::EditorialPitch", first_party)
        chase = model.split("Self::VerifyPlaylistPlacement { .. }", 1)[1].split(
            "ActionClass::FirstPartyReversible", 1
        )[0]
        self.assertIn("Self::EscalateEditorialPitch { .. }", chase)

    def test_the_deadline_precedes_the_release_and_the_chase_has_a_cooldown(self) -> None:
        rule = self.domain.split("pub fn evaluate_release", 1)[1].split("\n}", 1)[0]
        self.assertIn("editorial_pitch_days_before", rule)
        self.assertIn("editorial_pitch_escalate_within_days", rule)
        self.assertIn("editorial_pitch_escalation_cooldown_hours", rule)
        # A pitch window that outlives the release is a reminder about nothing.
        self.assertIn("until > Duration::ZERO", rule)
        # The parking threshold has to sit outside the chase window, or the
        # first chase lands before the task has been parked.
        valid = self.domain.split("const fn valid_policy", 1)[1].split("\n}", 1)[0]
        self.assertIn(
            "policy.editorial_pitch_days_before > policy.editorial_pitch_escalate_within_days",
            valid,
        )

    def test_a_chase_is_a_new_decision_and_not_a_repeat_of_the_last_one(self) -> None:
        # Keyed on the previous chase, so the next one is not deduplicated
        # against it and quietly dropped.
        candidate = read(CANDIDATE).split("fn editorial_pitch_escalation", 1)[1].split("\n}\n", 1)[0]
        keys = re.findall(r'(?:decision_key|action_idempotency_key): format!\(\s*"([^"]+)"', candidate)
        self.assertEqual(len(keys), 2)
        self.assertEqual(candidate.count("editorial_pitch_escalated_at"), 2)

    def test_the_chase_records_that_it_happened(self) -> None:
        # A reminder the schedule does not know about is a reminder every cycle.
        escalate = self.executor.split("async fn escalate_editorial_pitch", 1)[1].split("\n}", 1)[0]
        self.assertIn("SET editorial_pitch_escalated_at=$3", escalate)
        self.assertIn("editorial_pitch_completed_at IS NULL", escalate)

    def test_the_milestone_is_a_stored_value_the_database_admits(self) -> None:
        stored = re.search(r"milestone IN \((.*?)\)", self.sql, re.DOTALL)
        self.assertIsNotNone(stored)
        self.assertIn("editorial_pitch", stored.group(1))
        declared = self.domain.split("pub const fn template_key", 1)[1].split("\n    }", 1)[0]
        self.assertIn('"release.editorial_pitch.v1"', declared)

    def test_the_operator_has_one_way_to_say_it_is_done(self) -> None:
        openapi = read(OPENAPI)
        self.assertIn("/admin/autopilot/releases/{release_id}/editorial-pitch", openapi)
        self.assertIn("completeAutopilotEditorialPitch", openapi)


if __name__ == "__main__":
    unittest.main()
