"""Contract tests for the growth agent's volume envelope.

The class ceiling decides what kind of thing the agent may do alone; this
decides how much. These are the limits whose absence has a specific, expensive
failure mode -- an unbounded send, one fan hearing from four plays in a
morning, a wrong segment costing the whole list, no way to stop the agent
without a deploy -- so each one is pinned against being quietly removed.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
MIGRATION = ROOT / "migrations/0076_viryaos_growth_envelope.sql"
DOMAIN = ROOT / "crates/crowdrelay-domain/src/growth_envelope.rs"
EVALUATE = ROOT / "crates/crowdrelay-application/src/autopilot/evaluate.rs"
LOADER = ROOT / "crates/crowdrelay-infra/src/autopilot/decisions/opportunity_reads.rs"
PERSIST = ROOT / "crates/crowdrelay-infra/src/autopilot/decisions/persist.rs"
MODEL = ROOT / "crates/crowdrelay-application/src/autopilot/model.rs"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def strip_sql_comments(sql: str) -> str:
    return "\n".join(line.split("--", 1)[0] for line in sql.splitlines())


class GrowthEnvelopeContract(unittest.TestCase):
    def setUp(self) -> None:
        self.migration = read(MIGRATION)
        self.domain = read(DOMAIN)
        self.evaluate = read(EVALUATE)

    def test_a_new_workspace_starts_switched_off_and_rehearsing(self) -> None:
        # Both, not either: switching the agent on must not also be the moment
        # it first sends something real.
        schema = strip_sql_comments(self.migration)
        self.assertIn("agent_enabled boolean NOT NULL DEFAULT false", schema)
        self.assertIn("dry_run boolean NOT NULL DEFAULT true", schema)
        self.assertIn("a_new_workspace_has_an_agent_that_is_off_and_rehearsing", self.domain)

    def test_the_envelope_exists_for_existing_and_future_workspaces(self) -> None:
        self.assertIn("INSERT INTO viryaos_growth_envelope (workspace_id)", self.migration)
        self.assertIn("viryaos_provision_growth_envelope", self.migration)
        self.assertIn("AFTER INSERT ON workspaces", self.migration)

    def test_every_limit_is_bounded_in_the_schema(self) -> None:
        # An unbounded column is a cap an operator can remove by typing a large
        # number, which is the same as not having one.
        schema = strip_sql_comments(self.migration)
        for column in (
            "weekly_owned_audience_touches",
            "weekly_third_party_touches",
            "subject_cooldown_hours",
            "max_recipients_per_step",
        ):
            self.assertIn(column, schema)
            self.assertIn(f"CHECK ({column} BETWEEN", schema)

    def test_the_envelope_keeps_no_second_ledger(self) -> None:
        # Outward touches are already durable action rows. A second ledger is
        # one more thing that can disagree with what it claims to describe.
        schema = strip_sql_comments(self.migration)
        self.assertEqual(schema.count("CREATE TABLE"), 1)
        self.assertIn("CREATE TABLE viryaos_growth_envelope", schema)
        loader = read(LOADER)
        self.assertIn("FROM viryaos_autopilot_actions", loader)

    def test_spend_is_recorded_when_the_action_is_created(self) -> None:
        # Deriving the class at read time would mean reimplementing the Rust
        # classification in SQL, and the two would drift.
        self.assertIn("action_class", read(PERSIST))
        self.assertIn("candidate.action.action_class().as_str()", read(PERSIST))
        self.assertIn("ADD COLUMN action_class text", self.migration)

    def test_work_predating_the_agent_is_not_charged_to_it(self) -> None:
        # The column is nullable on purpose: NULL means "before the envelope".
        schema = strip_sql_comments(self.migration)
        self.assertIn("ADD COLUMN action_class text CHECK (action_class IS NULL", schema)
        loader = read(LOADER)
        self.assertIn("action_class IN ('owned_audience', 'third_party')", loader)

    def test_a_refused_approval_is_not_counted_as_a_touch(self) -> None:
        loader = read(LOADER)
        self.assertEqual(loader.count("status <> 'cancelled'"), 2)

    def test_both_counting_queries_are_indexed_and_time_bounded(self) -> None:
        self.assertIn("viryaos_autopilot_actions_outward_idx", self.migration)
        self.assertIn("viryaos_autopilot_actions_subject_outward_idx", self.migration)
        loader = read(LOADER)
        self.assertIn("INTERVAL '7 days'", loader)
        self.assertIn("INTERVAL '365 days'", loader)

    def test_a_missing_envelope_row_is_the_timid_default_not_an_absent_limit(
        self,
    ) -> None:
        loader = read(LOADER)
        self.assertIn("map_or_else(GrowthEnvelope::default", loader)
        self.assertIn("agent_enabled: false", self.domain)

    def test_the_envelope_applies_after_the_ceiling_never_instead_of_it(self) -> None:
        # A full budget must not let a third-party action through, and an empty
        # one must not promote anything.
        persist = self.evaluate.split("async fn persist(", 1)[1].split("\n    }", 1)[0]
        self.assertLess(
            persist.index("clamp_disposition(candidate.disposition, ceiling)"),
            persist.index("check_envelope("),
        )
        self.assertIn("EnvelopeVerdict::Allow => clamped", persist)

    def test_a_rehearsal_produces_nothing_anybody_can_press_send_on(self) -> None:
        persist = self.evaluate.split("async fn persist(", 1)[1].split("\n    }", 1)[0]
        self.assertIn("block.may_offer_for_approval()", persist)
        self.assertIn("AutonomyLevel::Recommend", persist)
        self.assertIn("a_rehearsal_never_produces_something_approvable", self.domain)

    def test_the_kill_switch_stops_contact_without_stopping_housekeeping(self) -> None:
        self.assertIn("if !class.is_outward()", self.domain)
        self.assertIn("the_kill_switch_stops_outward_contact_and_nothing_else", self.domain)

    def test_each_outward_class_spends_its_own_budget(self) -> None:
        # A busy newsletter week must not silence curator outreach.
        self.assertIn("each_outward_class_spends_its_own_budget", self.domain)

    def test_the_budget_boundary_is_pinned(self) -> None:
        # An off-by-one here is a send nobody authorised.
        self.assertIn("if spent >= budget", self.domain)
        self.assertIn("the_weekly_budget_stops_at_the_budget_not_after_it", self.domain)

    def test_the_cooldown_is_read_once_per_cycle_not_once_per_candidate(self) -> None:
        self.assertIn("load_outward_touch_ages", self.evaluate)
        execute = self.evaluate.split("pub async fn execute(", 1)[1].split(
            "async fn persist(", 1
        )[0]
        self.assertIn("load_outward_touch_ages", execute)
        self.assertIn("load_growth_envelope", execute)

    def test_the_budget_is_spent_as_it_is_used_not_read_once_per_cycle(self) -> None:
        # The cap is loaded once and would otherwise never move, so a single
        # cycle with fifty findings would enqueue all fifty against a budget of
        # five. Every one of them would be a send nobody authorised.
        persist = self.evaluate.split("async fn persist(", 1)[1].split("\n    }", 1)[0]
        self.assertIn("usage: &mut EnvelopeUsage", persist)
        spend = persist.split("if persisted.action_created {", 1)[1]
        self.assertIn("owned_audience_touches_7d.saturating_add(1)", spend)
        self.assertIn("third_party_touches_7d.saturating_add(1)", spend)
        self.assertIn("let (envelope, mut usage)", self.evaluate)

    def test_the_cooldown_applies_to_people_and_not_to_topics(self) -> None:
        # A show legitimately needs a listing sweep, an ambassador push and a
        # last-mile nudge over the weeks before it, each reaching different
        # people. A cooldown keyed on the event would allow one of them a week
        # and silently starve the rest.
        model = read(MODEL)
        self.assertIn("pub const fn is_contactable_person", model)
        persons = model.split("pub const fn is_contactable_person", 1)[1].split(
            "\n    }", 1
        )[0]
        for contact in ("Self::Fan(_)", "Self::BookingTarget(_)", "Self::OutreachTarget(_)", "Self::Beacon(_)"):
            self.assertIn(contact, persons)
        for topic in ("Self::Event(_)", "Self::ReleasePlan(_)", "Self::GrowthMetricSeries(_)"):
            self.assertNotIn(topic, persons)
        persist = self.evaluate.split("async fn persist(", 1)[1].split("\n    }", 1)[0]
        self.assertIn("is_contactable_person()", persist)
        self.assertIn("hours_since_subject_touched", persist)

    def test_a_gated_capability_is_a_state_rather_than_a_failing_cycle(self) -> None:
        # An operator who has switched a capability off has not broken
        # anything. Reporting it as a failed cycle every sixty seconds trains
        # everyone to ignore the log, and it also rolled back housekeeping that
        # needed no executor at all.
        team = read(ROOT / "crates/crowdrelay-infra/src/autopilot/team.rs")
        self.assertEqual(team.count("executor_capability_available"), 2)
        # Silent when nothing is parked; one line when work is actually waiting.
        self.assertIn("if !approvals.is_empty()", team)
        self.assertIn("if !rows.is_empty()", team)
        execution = read(ROOT / "crates/crowdrelay-infra/src/autopilot/execution.rs")
        available = execution.split("async fn executor_capability_available", 1)[1].split(
            "\n}", 1
        )[0]
        self.assertNotIn("tracing::warn", available)
        # The strict gate stays for the moment an action actually needs it.
        self.assertIn("ensure_executor_capability_strict", execution)

    def test_gated_work_is_parked_before_it_is_claimed(self) -> None:
        # Claiming an action spends one of its five attempts. Spending them on a
        # capability an operator has switched off retires the action for good:
        # once it is `failed` the snapshot no longer counts it as in flight, and
        # the re-proposed decision dedupes into the idempotency key it already
        # used. The work would then never run again, gate or no gate.
        actions = read(ROOT / "crates/crowdrelay-infra/src/autopilot/actions.rs")
        self.assertIn("partition_by_executor_capability", actions)
        self.assertIn("FOR UPDATE SKIP LOCKED", actions)
        park = actions.split("async fn park_gated_actions", 1)[1].split("\n}", 1)[0]
        self.assertIn("status = 'queued'", park)
        self.assertIn("last_error_kind = 'awaiting_executor'", park)
        # Parking must not look like an attempt, so nothing here may touch the
        # attempt counter.
        self.assertNotIn("attempt_count", park)
        self.assertIn("no executor advertises this capability", park)

    def test_every_capability_an_action_needs_is_one_an_executor_can_advertise(self) -> None:
        # The pre-claim check and the emission-time check must name capabilities
        # identically, or an action is parked under one name and gated under
        # another.
        execution = read(ROOT / "crates/crowdrelay-infra/src/autopilot/execution.rs")
        by_payload = execution.split("fn executor_capability_for_payload", 1)[1].split(
            "\nfn executor_capability_for_event", 1
        )[0]
        by_event = execution.split("fn executor_capability_for_event", 1)[1].split("\n}", 1)[0]
        advertised = set(re.findall(r'=> "([a-z][a-z0-9_.]+)"', by_event))
        for capability in set(re.findall(r'=> "([a-z][a-z0-9_.]+)"', by_payload)):
            self.assertIn(capability, advertised)

    def test_held_decisions_are_counted_apart_from_gated_and_throttled_ones(self) -> None:
        report = self.evaluate.split("pub struct AutopilotCycleReport {", 1)[1].split(
            "}", 1
        )[0]
        for counter in ("actions_held", "actions_gated", "actions_throttled"):
            self.assertIn(counter, report)


if __name__ == "__main__":
    unittest.main()
