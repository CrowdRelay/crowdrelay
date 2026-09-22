#!/usr/bin/env python3
"""Source contract for suggestion-queue retention.

The approval queue's deadline is only real if something enforces it
everywhere. The sweep used to live inside the action claim path, which runs
per autopilot cycle — so a parked tenant, a disabled autopilot or a starved
claim loop accumulated asks that read `awaiting_approval` long past
`approval_expires_at`, hidden from `needs_you` and counted only as
`awaiting_sweep` by the lapsed read. And `autopilot_decisions` grew
forever, because nothing ever deleted a row.

These assertions pin the shape of the fix rather than re-testing its
arithmetic: one shared sweep with two callers (the claim path's fast lane
and the retention worker's global lane), a bounded audit horizon for
decisions that produced nothing, and a watchdog condition that fires when
the queue's dead outlive two sweep intervals. Each one compiles and tests
clean when broken again, which is why the gate is here rather than in the
type system.
"""
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

ACTIONS = ROOT / "crates/crowdrelay-infra/src/autopilot/actions.rs"
SWEEP = ROOT / "crates/crowdrelay-infra/src/autopilot/lapsed_sweep.rs"
MOD = ROOT / "crates/crowdrelay-infra/src/autopilot.rs"
RETENTION = ROOT / "crates/crowdrelay-worker/src/retention.rs"
STEPS = ROOT / "crates/crowdrelay-worker/src/retention/steps.rs"
WATCHDOG = ROOT / "crates/crowdrelay-worker/src/ops_watchdog/conditions.rs"
LAPSED = ROOT / "crates/crowdrelay-infra/src/lapsed_approvals.rs"


class SuggestionRetentionV1Contract(unittest.TestCase):
    def test_the_sweep_is_one_shared_function(self):
        # Two callers, one implementation. If the claim path keeps its own
        # copy of the expiry UPDATE, the retention pass can silently disagree
        # with it — the drift this file exists to prevent.
        sweep = SWEEP.read_text()
        self.assertIn("pub async fn sweep_lapsed_approval_asks", sweep)
        # The cascades are part of the sweep, not of either caller: a dead
        # ask resolves its suggestion and arc whoever reaps it.
        self.assertIn("content_suggestions", sweep)
        self.assertIn("suggestion_outcomes", sweep)
        self.assertIn("arcs", sweep)

    def test_the_claim_path_delegates(self):
        actions = ACTIONS.read_text()
        self.assertIn("sweep_lapsed_approval_asks", actions)
        # The inlined approval-expiry UPDATE must not come back alongside
        # the call — two writers of the same transition is how the kinds of
        # death start disagreeing.
        self.assertNotIn("last_error_kind='approval_expired'", actions)

    def test_the_sweep_is_public_for_the_retention_worker(self):
        mod = MOD.read_text()
        self.assertIn("mod lapsed_sweep", mod)
        self.assertIn("pub use lapsed_sweep", mod)

    def test_retention_runs_the_sweep_globally(self):
        retention = RETENTION.read_text()
        steps = STEPS.read_text()
        self.assertIn("RetentionStep::LapsedAutopilotAsks", retention)
        self.assertIn("lapsed_autopilot_asks_swept", retention)
        # `None` workspace is the whole point — the claim path passes its
        # own; retention must not.
        self.assertIn("sweep_lapsed_approval_asks", steps)

    def test_orphan_decisions_have_an_audit_horizon(self):
        retention = RETENTION.read_text()
        steps = STEPS.read_text()
        self.assertIn("RetentionStep::OldOrphanAutopilotDecisions", retention)
        self.assertIn("decision_audit_retention", retention)
        # Both RESTRICT referrers must be guarded, not just the obvious one.
        self.assertIn("autopilot_actions", steps)
        self.assertIn("autopilot_outcomes", steps)
        self.assertIn("DELETE FROM autopilot_decisions", steps)

    def test_the_deadline_contract_has_a_watchdog(self):
        conditions = WATCHDOG.read_text()
        self.assertIn("approval.sweep_lagging", conditions)
        self.assertIn("unswept_lapsed_approvals", conditions)

    def test_the_lapsed_read_names_the_retention_sweep(self):
        # `awaiting_sweep` means "no sweep reached it" — the doc must not go
        # back to describing a claim-path-only world.
        doc = LAPSED.read_text()
        self.assertIn("retention", doc)


if __name__ == "__main__":
    unittest.main()
