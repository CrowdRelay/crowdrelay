"""Contract tests for the daily operator brief.

Every read model a brief needs already existed and nothing ever sent one. The
production state that made this worth building was an agent with its envelope
disabled and a dozen decisions awaiting approval, which from outside is
indistinguishable from a quiet week.

So the properties pinned here are the ones whose absence would restore that
silence, or turn the brief into something an operator filters away:

- it does not run behind the growth envelope, or a disabled agent silences the
  message saying the agent is disabled;
- it is not gated on `autopilot_enabled` for the same reason;
- silence is the default, with named exceptions;
- the record and the delivery are written in one transaction;
- `last_brief_at` is durable rather than read from a table retention prunes.
"""

from pathlib import Path
import json
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
MIGRATION = ROOT / "migrations/0084_viryaos_operator_brief.sql"
DOMAIN = ROOT / "crates/crowdrelay-domain/src/operator_brief.rs"
WORKER = ROOT / "crates/crowdrelay-worker/src/operator_brief.rs"
MAIN = ROOT / "crates/crowdrelay-worker/src/main.rs"
ROUTES = ROOT / "ops/edge/routes.json"
BRIDGE = ROOT / "ops/edge/bridge.js"
CONTRACT = ROOT / "n8n/viryaos-executor-contract.md"

EVENT_TYPE = "viryaos.ops.operator_brief"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


class OperatorBriefContract(unittest.TestCase):
    def setUp(self) -> None:
        self.domain = read(DOMAIN)
        self.worker = read(WORKER)

    def headlines(self) -> set[str]:
        block = self.domain.split("impl BriefHeadline", 1)[1].split("\n    }", 1)[0]
        return set(re.findall(r'Self::\w+ => "([a-z_]+)"', block))

    def test_the_stored_headline_set_matches_the_rule(self) -> None:
        constraint = re.search(
            r"headline text NOT NULL CHECK \(headline IN \((.*?)\)\)",
            read(MIGRATION),
            re.DOTALL,
        )
        self.assertIsNotNone(constraint)
        self.assertEqual(
            set(re.findall(r"'([a-z_]+)'", constraint.group(1))),
            self.headlines(),
            "a headline the rule can emit but the table refuses fails at send time",
        )

    def test_silence_is_the_default_and_the_exceptions_are_named(self) -> None:
        rule = self.domain.split("pub fn evaluate_operator_brief", 1)[1]
        self.assertIn("NothingWorthSaying", rule)
        self.assertIn("warrants_interrupting", rule)
        self.assertIn("a_quiet_day_with_nothing_waiting_says_nothing", self.domain)

    def test_a_disabled_agent_with_work_waiting_still_produces_a_brief(self) -> None:
        # The one silence that actively misleads.
        self.assertIn("DisabledWithWorkWaiting", self.domain)
        self.assertIn(
            "an_agent_switched_off_with_work_waiting_is_the_silence_that_must_break",
            self.domain,
        )
        self.assertIn(
            "an_agent_switched_off_with_an_empty_queue_is_a_decision_not_a_problem",
            self.domain,
        )

    def test_the_interval_is_checked_before_any_headline_is_chosen(self) -> None:
        rule = self.domain.split("pub fn evaluate_operator_brief", 1)[1]
        self.assertLess(
            rule.index("IntervalNotElapsed"),
            rule.index("let headline"),
            "a brief must never be re-sent inside its interval, however bad the news",
        )
        self.assertIn(
            "nothing_is_sent_twice_inside_the_interval_however_bad_the_news", self.domain
        )

    def test_the_brief_does_not_run_behind_the_growth_envelope(self) -> None:
        # Routing it through the envelope means a disabled envelope silences the
        # message whose job is to report that the envelope is disabled.
        for forbidden in ("check_envelope", "EnvelopeVerdict", "AutopilotContext"):
            self.assertNotIn(forbidden, self.worker)

    def test_the_brief_is_not_gated_on_autopilot_being_enabled(self) -> None:
        main = read(MAIN)
        spawn = main.split("OperatorBriefWorker::new", 1)[1].split(";", 1)[0]
        self.assertNotIn("autopilot_enabled", spawn)
        self.assertIn("OPERATOR_BRIEF_INTERVAL", spawn)

    def test_the_record_and_the_delivery_are_one_transaction(self) -> None:
        send = self.worker.split("async fn send(", 1)[1].split("\n    }", 1)[0]
        self.assertIn("begin()", send)
        self.assertIn("viryaos_operator_briefs", send)
        self.assertIn("outbox_events", send)
        self.assertIn("transaction.commit()", send)

    def test_last_brief_at_is_read_from_durable_state_not_the_outbox(self) -> None:
        # Outbox rows are pruned by retention, and an idempotency guarantee that
        # expires with a retention window is not one.
        snapshot = self.worker.split("async fn snapshot(", 1)[1].split("\n    }", 1)[0]
        self.assertIn("FROM viryaos_operator_briefs", snapshot)
        self.assertNotIn("FROM outbox_events", snapshot)

    def test_an_unconfigured_workspace_is_never_reported_as_a_running_agent(
        self,
    ) -> None:
        snapshot = self.worker.split("async fn snapshot(", 1)[1].split("\n    }", 1)[0]
        self.assertIn("agent_enabled: row.agent_enabled.unwrap_or(false)", snapshot)
        self.assertIn("dry_run: row.dry_run.unwrap_or(true)", snapshot)

    def test_parked_work_is_counted_apart_from_the_approval_queue(self) -> None:
        # Approving a parked action changes nothing: no executor advertises the
        # capability. Merging the two sends the operator to work that cannot move.
        snapshot = self.worker.split("async fn snapshot(", 1)[1].split("\n    }", 1)[0]
        self.assertIn("last_error_kind='awaiting_executor'", snapshot)
        self.assertIn(
            "parked_work_outranks_an_approval_queue_a_human_can_actually_clear",
            self.domain,
        )

    def test_the_brief_states_facts_and_never_gives_instructions(self) -> None:
        self.assertIn(
            "every_headline_produces_a_summary_that_states_a_fact", self.worker
        )

    def test_the_event_is_routable_at_the_edge(self) -> None:
        routes = json.loads(read(ROUTES))
        self.assertIn(
            EVENT_TYPE,
            routes,
            "an unrouted event type is rejected 422 and dead-letters",
        )
        count = re.search(r"Object\.keys\(routes\)\.length !== (\d+)", read(BRIDGE))
        self.assertIsNotNone(count)
        self.assertEqual(
            int(count.group(1)),
            len(routes),
            "the bridge's asserted route count drifted from the route map",
        )

    def test_the_executor_contract_documents_the_new_event(self) -> None:
        self.assertIn(EVENT_TYPE, read(CONTRACT))

    def test_the_domain_holds_no_provider_or_sql_concept(self) -> None:
        for forbidden in ("sqlx", "reqwest", "SELECT ", "INSERT "):
            self.assertNotIn(
                forbidden, self.domain, f"domain module leaked {forbidden!r}"
            )


if __name__ == "__main__":
    unittest.main()
