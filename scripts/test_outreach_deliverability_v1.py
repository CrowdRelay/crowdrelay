"""Contract tests for outreach deliverability — the ramp and the halt.

Deliverability is not a detail. A burned sending domain does not degrade the
outreach channel, it ends it, and it takes the transactional mail sharing that
domain with it. Two properties decide whether the channel survives:

1. Volume ramps. The operator's weekly third-party budget is a ceiling, not a
   target; a workspace still earning its reputation sends less than the
   ceiling, and a standing start reads to a receiving provider as exactly
   what it looks like.
2. A rising bounce or complaint rate stops the wave. Not reports it after —
   by the time a digest is read, the reputation is already spent. The halt is
   a precondition of sending.

And one address-level rule: only a hard bounce finishes an address, through
the suppression that already exists rather than a second mechanism invented
here.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
MIGRATION = ROOT / "migrations/0101_viryaos_outreach_delivery_faults.sql"
DOMAIN = ROOT / "crates/crowdrelay-domain/src/deliverability.rs"
INFRA_READ = ROOT / "crates/crowdrelay-infra/src/autopilot/deliverability.rs"
ACTIONS = ROOT / "crates/crowdrelay-infra/src/autopilot/actions_execution.rs"
EVALUATE = ROOT / "crates/crowdrelay-application/src/autopilot/evaluate.rs"
PORTS = ROOT / "crates/crowdrelay-application/src/autopilot/ports.rs"
STATE_PORTS = ROOT / "crates/crowdrelay-application/src/autopilot/control/state_ports.rs"
API = ROOT / "crates/crowdrelay-api/src/autopilot/deliverability.rs"
ROUTING = ROOT / "crates/crowdrelay-api/src/routing.rs"
OPENAPI = ROOT / "openapi/openapi.yaml"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def strip_sql_comments(sql: str) -> str:
    return "\n".join(line.split("--", 1)[0] for line in sql.splitlines())


class DeliverabilityContract(unittest.TestCase):
    def setUp(self) -> None:
        self.sql = strip_sql_comments(read(MIGRATION))
        self.domain = read(DOMAIN)
        self.infra = read(INFRA_READ)
        self.actions = read(ACTIONS)

    def test_faults_are_rows_so_a_rate_can_be_computed_over_a_window(self) -> None:
        # One row per fault with its own timestamp: a rate over a rolling
        # window, not a counter somebody has to reset.
        self.assertIn("CREATE TABLE viryaos_outreach_delivery_faults", self.sql)
        self.assertRegex(self.sql, r"fault text NOT NULL CHECK \(fault IN \('hard_bounce', 'soft_bounce', 'complaint'\)\)")
        self.assertIn(
            "CREATE INDEX viryaos_outreach_delivery_faults_window_idx",
            self.sql,
            "the rate is read on every cycle that wants to send",
        )

    def test_a_retried_webhook_is_a_replay_not_a_second_halt(self) -> None:
        # The unique constraint plus `DO NOTHING` is what makes provider
        # retries idempotent at the ledger level.
        self.assertIn("UNIQUE (workspace_id, provider_reference)", self.sql)
        self.assertIn("ON CONFLICT (workspace_id, provider_reference) DO NOTHING", self.infra)
        self.assertIn("replayed: true", self.infra)

    def test_the_denominator_is_sends_not_actions(self) -> None:
        # An action queued and never dispatched counts toward nothing, or a
        # broken executor reads as a bounce problem.
        self.assertIn("action.status = 'succeeded'", self.infra)
        self.assertIn("action_class = 'third_party'", self.infra)

    def test_the_ramp_clock_starts_when_the_first_send_actually_leaves(self) -> None:
        # Written in the same transaction that marks the third-party action
        # succeeded, so the clock can never be ahead of the evidence.
        block = self.actions.split("if action.payload.action_class() == ActionClass::ThirdParty", 1)[1]
        block = block.split("transaction.commit()", 1)[0]
        self.assertIn("first_third_party_send_at = COALESCE(first_third_party_send_at, $2)", block)
        self.assertIn("UPDATE viryaos_autopilot_actions\n                SET status = 'succeeded'", self.actions)

    def test_only_a_hard_bounce_suppresses_and_through_the_existing_flag(self) -> None:
        domain = self.domain.split("pub const fn suppresses_target", 1)[1].split("\n    }", 1)[0]
        self.assertNotIn("Complaint", domain, "a complaint is about the message, not the address")
        infra = self.infra.split("if command.fault.suppresses_target()", 1)[1].split("\n            transaction.commit()", 1)[0]
        self.assertIn("UPDATE viryaos_outreach_targets", infra)
        self.assertIn("accepts_outreach = false", infra)
        # No second suppression mechanism is invented here.
        self.assertNotIn("do_not_contact = true", infra)

    def test_the_halt_is_a_precondition_of_sending_not_a_report_afterwards(self) -> None:
        evaluate = (
            read(ROOT / "crates/crowdrelay-application/src/autopilot/evaluate/outreach_supply.rs")
            .split("load_deliverability_snapshot", 1)[1]
            .split("\n        for anchor", 1)[0]
        )
        self.assertIn("ramped_ceiling", evaluate)
        self.assertIn(".min(envelope.weekly_third_party_touches)", evaluate)
        domain = self.domain.split("pub fn ramped_ceiling", 1)[1].split("\n}", 1)[0]
        # Zero is a real answer: a halted workspace may not send at all.
        self.assertIn("!verdict(snapshot, policy).sending_allowed()", domain)
        self.assertIn("return 0;", domain)

    def test_complaints_outrank_bounces_when_both_are_failing(self) -> None:
        verdict = self.domain.split("pub fn verdict", 1)[1].split("\n}", 1)[0]
        complaint_at = verdict.find("HaltComplaintRate")
        bounce_at = verdict.find("HaltBounceRate")
        self.assertGreater(complaint_at, -1)
        self.assertLess(complaint_at, bounce_at, "the more expensive signal is named first")

    def test_a_rate_below_the_sample_floor_is_not_a_rate(self) -> None:
        verdict = self.domain.split("pub fn verdict", 1)[1].split("\n}", 1)[0]
        self.assertIn("snapshot.sent_30d < policy.minimum_rate_sample", verdict)
        self.assertIn("return DeliverabilityVerdict::Healthy;", verdict.split("minimum_rate_sample", 1)[1])

    def test_the_ramp_never_exceeds_the_operators_own_budget(self) -> None:
        ramp = self.domain.split("pub fn ramped_ceiling", 1)[1].split("\n}", 1)[0]
        self.assertIn(".min(snapshot.weekly_third_party_ceiling)", ramp)

    def test_ingestion_is_on_the_internal_surface_with_an_idempotency_key(self) -> None:
        routing = read(ROUTING)
        self.assertIn('/v1/internal/autopilot/outreach/candidates', routing)
        self.assertIn("/v1/internal/autopilot/outreach/delivery-faults", routing)
        handler = read(API)
        self.assertIn("commerce_authorized", handler, "executor work uses the commerce key")
        self.assertIn("parse_idempotency_key", handler)
        self.assertIn("deny_unknown_fields", handler)

    def test_a_fault_dated_outside_its_window_is_refused_not_clamped(self) -> None:
        # The halt is only as honest as the dates it is computed from: a
        # complaint parked a year out would pollute the rolling window for
        # months, and one silently clamped is a number nobody can argue with.
        handler = read(API)
        self.assertIn("MAX_DELIVERY_FAULT_AGE", handler)
        self.assertIn("DELIVERY_FAULT_CLOCK_SKEW", handler)
        self.assertIn("return Err(())", handler.split("request.occurred_at > now + DELIVERY_FAULT_CLOCK_SKEW", 1)[1][:200])

    def test_webhooks_may_address_by_email_exactly_one_of_two_fields(self) -> None:
        # Providers report addresses, not CrowdRelay ids. Both fields at once
        # is ambiguous; neither is unaddressable; both are refusals.
        handler = read(API)
        self.assertIn("DeliveryFaultSubject::Target(", handler)
        self.assertIn("DeliveryFaultSubject::ContactEmail(", handler)
        self.assertIn("_ => return Err(())", handler)

    def test_openapi_documents_the_route(self) -> None:
        openapi = read(OPENAPI)
        self.assertIn("/internal/autopilot/outreach/delivery-faults", openapi)
        self.assertIn("recordAutopilotDeliveryFault", openapi)
        self.assertIn("DeliveryFaultRequest:", openapi)
        self.assertIn("enum: [hard_bounce, soft_bounce, complaint]", openapi)

    def test_application_keeps_zero_sqlx_call_sites_for_this_path(self) -> None:
        for path in (PORTS, STATE_PORTS):
            self.assertNotIn("sqlx", read(path))


if __name__ == "__main__":
    unittest.main()
