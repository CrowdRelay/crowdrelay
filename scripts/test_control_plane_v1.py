"""Contract tests for the control plane — find, then "do it".

The operator's no-autonomy mode. The agent finds and parks; a human decides.
Three things make that honest:

1. "Do it" approves through the existing approval endpoint. One button, one
   action, no new authority path — the control plane must never become a
   second way to execute anything.
2. "Done ourselves" records that a human handled the finding outside the
   system. It is a first-class outcome, not a dismissal: an opportunity a
   human took is a success, and recording it as ignored would teach the
   ranker the wrong thing.
3. A handled finding stops being proposed. The queue reads the ledger row,
   so nothing can disagree about whether this was handled.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
QUEUE = ROOT / "crates/crowdrelay-infra/src/autopilot/operations/next_best_action.rs"
CONTROL = ROOT / "crates/crowdrelay-infra/src/autopilot/control_mutations.rs"
VIEW = ROOT / "crates/crowdrelay-application/src/autopilot/control.rs"
PORTS = ROOT / "crates/crowdrelay-application/src/autopilot/control/runtime_ports.rs"
API = ROOT / "crates/crowdrelay-api/src/autopilot/experiments_actions.rs"
ROUTING = ROOT / "crates/crowdrelay-api/src/routing.rs"
OPENAPI = ROOT / "openapi/openapi.yaml"

APPROVE_ROUTE = "/v1/admin/autopilot/actions/{action_id}/approve"
HANDLED_ROUTE = "/v1/admin/autopilot/decisions/{decision_id}/handled-externally"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


class ControlPlaneContract(unittest.TestCase):
    def setUp(self) -> None:
        self.routing = read(ROUTING)
        self.control = read(CONTROL)

    def test_do_it_is_the_existing_approval_endpoint(self) -> None:
        # One button over one action through one authority path. A control
        # plane with its own way to execute would be a second source of truth
        # wearing the same clothes.
        self.assertIn(f'"{APPROVE_ROUTE}"', self.routing)
        handler = read(API)
        self.assertNotIn("handle_autopilot_decision_externally", handler.split("mark_decision_handled_externally", 1)[0])

    def test_done_ourselves_is_a_first_class_outcome_not_a_dismissal(self) -> None:
        block = self.control.split("async fn mark_decision_handled_operator", 1)[1].split("\n    async fn", 1)[0]
        # The ledger row names the outcome, so the record can be read as a
        # success rather than as work nobody wanted.
        self.assertIn('"handle_autopilot_decision_externally"', block)
        self.assertIn('"outcome": "handled_by_human"', block)
        self.assertNotIn("disposition = ", block, "the decision row is history; nothing rewrites it")

    def test_a_handled_finding_stops_being_proposed(self) -> None:
        queue = read(QUEUE)
        self.assertIn("'handle_autopilot_decision_externally'", queue)
        exclusion = queue.split("AND NOT EXISTS (\n              SELECT 1 FROM operator_actions AS handled", 1)[1].split(")", 1)[0]
        self.assertIn("target_type = 'autopilot_decision'", exclusion)
        self.assertIn("target_id = decision.id", exclusion)

    def test_parked_work_of_a_handled_finding_is_withdrawn_in_the_same_transaction(self) -> None:
        block = self.control.split("async fn mark_decision_handled_operator", 1)[1].split("\n    async fn", 1)[0]
        withdraw = block.split("UPDATE viryaos_autopilot_actions", 1)[1].split("transaction.commit()", 1)[0]
        self.assertIn("status = 'cancelled'", withdraw)
        self.assertIn("status = 'awaiting_approval'", withdraw)
        # And only after the ledger row exists, so a crash between them leaves
        # either both or neither.
        self.assertLess(
            block.find('"handle_autopilot_decision_externally"'),
            block.find("UPDATE viryaos_autopilot_actions"),
        )

    def test_handling_requires_a_real_finding(self) -> None:
        # A suppression row for a decision nobody ever saw would silently hide
        # whatever lands on that id later.
        block = self.control.split("async fn mark_decision_handled_operator", 1)[1].split("\n    async fn", 1)[0]
        guard = block.split("SELECT EXISTS (", 1)[1].split(")", 1)[0]
        self.assertIn("viryaos_autopilot_decisions", guard)
        self.assertIn("RepositoryError::NotFound", block)

    def test_board_entries_address_real_rows(self) -> None:
        view = read(VIEW).split("pub struct NextBestAction", 1)[1].split("\n}", 1)[0]
        self.assertIn("pub decision_id: uuid::Uuid", view)
        self.assertIn("pub action_id: Option<uuid::Uuid>", view)
        sql = read(QUEUE).split("SELECT\n            decision.id AS decision_id", 1)[1][:400]
        self.assertIn("action.action_id", sql)

    def test_replay_is_reported_as_a_replay(self) -> None:
        block = self.control.split("async fn mark_decision_handled_operator", 1)[1].split("\n    async fn", 1)[0]
        self.assertIn("replayed: true", block)

    def test_openapi_documents_the_route(self) -> None:
        openapi = read(OPENAPI)
        self.assertIn("/admin/autopilot/decisions/{decision_id}/handled-externally", openapi)
        self.assertIn("markAutopilotDecisionHandledExternally", openapi)
        schema = openapi.split("    NextBestAction:", 1)[1][:1400]
        self.assertRegex(schema, r"required: \[position, decision_id,")


if __name__ == "__main__":
    unittest.main()
