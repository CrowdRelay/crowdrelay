#!/usr/bin/env python3
"""Regression contract for the dedicated VIRYA OS team-email dispatch lane."""

from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]


def text(path: str) -> str:
    return (ROOT / path).read_text()


class TeamEmailDispatchLaneContract(unittest.TestCase):
    def test_team_email_has_a_dedicated_claim_scope(self):
        actions = text("crates/crowdrelay-infra/src/autopilot/actions.rs")
        self.assertIn('const TEAM_ASSIGNMENT_EMAIL_ACTION_KIND: &str = "team.assignment.email";', actions)
        self.assertIn("claim_due_team_email_actions", actions)
        self.assertIn("Some(TEAM_ASSIGNMENT_EMAIL_ACTION_KIND),\n            None", actions)
        self.assertIn("claim_due_autonomous_actions", actions)
        self.assertIn("None,\n            Some(TEAM_ASSIGNMENT_EMAIL_ACTION_KIND)", actions)
        self.assertIn("AND ($4::text IS NULL OR action_kind = $4)", actions)
        self.assertIn("AND ($5::text IS NULL OR action_kind <> $5)", actions)

    def test_worker_routes_team_email_separately_from_autonomous_actions(self):
        worker = text("crates/crowdrelay-worker/src/autopilot.rs")
        self.assertIn("pub struct TeamEmailDispatchWorker", worker)
        self.assertIn("claim_due_team_email_actions", worker)
        self.assertIn("claim_due_autonomous_actions", worker)
        self.assertIn("dispatch_team_handoff_reminders", worker)

    def test_team_email_lane_is_not_gated_by_autopilot_enabled(self):
        main = text("crates/crowdrelay-worker/src/main.rs")
        team_worker = main.index("let team_email_worker = TeamEmailDispatchWorker::new")
        autopilot_gate = main.index("let autopilot_worker = if config.autopilot_enabled")
        self.assertLess(team_worker, autopilot_gate)
        self.assertIn("team_email_worker.run(team_email_shutdown).await", main)
        self.assertIn("config.autopilot_poll_interval.min(Duration::from_secs(60))", main)

    def test_dispatch_event_and_capability_mapping_are_closed_loop(self):
        actions = text("crates/crowdrelay-infra/src/autopilot/actions_execution.rs")
        execution = text("crates/crowdrelay-infra/src/autopilot/execution.rs")
        self.assertIn('"crowdrelay.team.assignment_email_requested"', actions)
        self.assertIn('"crowdrelay.team.assignment_email_requested" => "team.email"', execution)
        self.assertIn("SendTeamAssignmentEmail", execution)
        self.assertIn("payload_requires_executor", actions)

    def test_postgres_regression_is_in_canonical_ci(self):
        integration = text("crates/crowdrelay-infra/tests/autopilot_team_email_postgres.rs")
        ci = text(".github/workflows/ci.yml")
        self.assertIn("claim_due_team_email_actions", integration)
        self.assertIn("crowdrelay.team.assignment_email_requested", integration)
        self.assertIn('assert_eq!(action_status, "succeeded")', integration)
        self.assertIn("autopilot_team_email_postgres", ci)
        self.assertIn("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL", ci)

    def test_provider_success_is_monotonic_under_delayed_failure_receipts(self):
        runtime = text("crates/crowdrelay-infra/src/autopilot/runtime.rs")
        integration = text("crates/crowdrelay-infra/tests/autopilot_team_email_postgres.rs")
        self.assertIn("preserve_succeeded_claim", runtime)
        self.assertIn('claim_status == "succeeded"', runtime)
        self.assertIn("delayed-failure-", integration)
        self.assertIn('assert_eq!(after_success.disposition, "already_succeeded")', integration)
        self.assertIn('assert_eq!(claim_state.0, "succeeded")', integration)


if __name__ == "__main__":
    unittest.main()
