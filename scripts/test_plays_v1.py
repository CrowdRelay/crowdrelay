"""Contract tests for plays — the agent's first stateful context.

Every context before this one answers a question per cycle and forgets. That
shape is right for a detector and wrong for a campaign: the growth work that
matters is a message when a date near a fan is announced, a second one the
morning after they enjoyed it, and a reading of whether either moved anything.
Without durable state, step two either never happens or happens every cycle.

The properties pinned here are the ones whose absence would turn that into a
mailing machine rather than a campaign:

- a step's authority comes from what the step *is*, not from a column somebody
  filled in;
- an omission is written down, so "we did not send it" is a fact rather than an
  absence somebody has to notice;
- a fan is reached by a step once, enforced by the database and not by a query
  the evaluator has to remember;
- consent is checked at the moment of sending, not at the moment of deciding;
- a play that cannot deliver still finishes, instead of holding a window open
  against nobody.

As the newest migration to define the autopilot context set, this file also
owns the claim that the database, the provisioning trigger and the Rust enum
still agree. That claim belongs to whichever migration defines it last: pinned
to the one that first introduced a context, it would keep passing against a set
the database had stopped enforcing.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
MIGRATIONS = ROOT / "migrations"
MIGRATION = MIGRATIONS / "0088_viryaos_plays.sql"
DOMAIN = ROOT / "crates/crowdrelay-domain/src/plays.rs"
MODEL = ROOT / "crates/crowdrelay-application/src/autopilot/model.rs"
EVALUATE = ROOT / "crates/crowdrelay-application/src/autopilot/evaluate.rs"
CANDIDATE = ROOT / "crates/crowdrelay-application/src/autopilot/evaluate/plays.rs"
PORTS = ROOT / "crates/crowdrelay-application/src/autopilot/ports.rs"
VALIDATION = ROOT / "crates/crowdrelay-api/src/autopilot/validation.rs"
MAPPING = ROOT / "crates/crowdrelay-infra/src/autopilot/mapping.rs"
EXECUTION = ROOT / "crates/crowdrelay-infra/src/autopilot/execution.rs"
ACTIONS = ROOT / "crates/crowdrelay-infra/src/autopilot/actions.rs"
INFRA = ROOT / "crates/crowdrelay-infra/src/autopilot/plays.rs"
CONTRACT = ROOT / "n8n/viryaos-executor-contract.md"
OPENAPI = ROOT / "openapi/openapi.yaml"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def strip_sql_comments(sql: str) -> str:
    return "\n".join(line.split("--", 1)[0] for line in sql.splitlines())


class PlaysContract(unittest.TestCase):
    def setUp(self) -> None:
        self.migration = read(MIGRATION)
        self.sql = strip_sql_comments(self.migration)
        self.domain = read(DOMAIN)
        self.infra = read(INFRA)

    def contexts(self) -> set[str]:
        model = read(MODEL)
        block = model.split("impl AutopilotContext", 1)[1].split(
            "/// Typed bounded-context", 1
        )[0]
        return set(re.findall(r'Self::\w+ => "([a-z0-9_]+)"', block))

    def stored_contexts(self) -> list[set[str]]:
        """The three context constraints as the newest migration leaves them."""
        latest: list[str] = []
        for path in sorted(MIGRATIONS.glob("*.sql")):
            found = re.findall(
                r"ADD CONSTRAINT viryaos_autopilot_\w+_context_check CHECK \(context IN \((.*?)\)\)",
                read(path),
                re.DOTALL,
            )
            if found:
                latest = found
        self.assertEqual(
            len(latest), 3, "policies, decisions and actions must all be constrained"
        )
        return [set(re.findall(r"'([a-z0-9_]+)'", block)) for block in latest]

    def provisioned_contexts(self) -> set[str]:
        """The trigger's context list as the newest migration replaces it."""
        latest: str | None = None
        for path in sorted(MIGRATIONS.glob("*.sql")):
            text = read(path)
            if "CREATE OR REPLACE FUNCTION viryaos_provision_autopilot_policies" in text:
                latest = text.split(
                    "CREATE OR REPLACE FUNCTION viryaos_provision_autopilot_policies", 1
                )[1]
        self.assertIsNotNone(latest, "no migration provisions autopilot policies")
        return set(re.findall(r"NEW\.id, '([a-z0-9_]+)'", latest))

    # --- the claim this file inherits -----------------------------------

    def test_the_stored_context_set_matches_the_rust_enum(self) -> None:
        for allowed in self.stored_contexts():
            self.assertEqual(
                allowed,
                self.contexts(),
                "database context constraint drifted from AutopilotContext",
            )

    def test_a_workspace_created_later_also_gets_every_context(self) -> None:
        # The backfill covers today's workspaces and the trigger covers
        # tomorrow's. Updating only one works perfectly until the next
        # workspace is created.
        self.assertIn("SELECT workspace.id, 'plays', 40", self.migration)
        self.assertEqual(
            self.provisioned_contexts(),
            self.contexts(),
            "the provisioning trigger drifted from AutopilotContext",
        )

    def test_the_context_is_parseable_everywhere_a_context_is_read(self) -> None:
        # A context the policy table can hold but a reader cannot parse fails
        # the whole overview read, not just its own row.
        self.assertIn('"plays" => AutopilotContext::Plays', read(MAPPING))
        self.assertIn('"plays" => Some(AutopilotContext::Plays)', read(VALIDATION))
        self.assertIn("plays]", read(OPENAPI))

    def test_the_context_arrives_disabled_and_quota_limited(self) -> None:
        # A play that starts running because a migration landed is a play
        # nobody chose to run.
        provisioning = self.migration.split("INSERT INTO viryaos_autopilot_policies", 1)[1]
        columns = re.search(r"^\s*\(([^)]*)\)", provisioning)
        self.assertIsNotNone(columns)
        self.assertEqual(
            {column.strip() for column in columns.group(1).split(",")},
            {"workspace_id", "context", "max_actions_24h"},
            "a new context must inherit the disabled/observe defaults",
        )

    # --- what a play is -------------------------------------------------

    def test_a_step_takes_its_authority_from_what_it_is(self) -> None:
        # A play author who could choose the class could route a curator email
        # through a step the operator only approved for their own fans.
        self.assertIn("pub const fn action_class(self) -> ActionClass", self.domain)
        specs = self.domain.split("const TRACK_US_ASK_STEPS", 1)[1].split("];", 1)[0]
        self.assertNotIn("class: ActionClass::", specs)
        self.assertIn("class: PlayStepKind::", specs)
        payload = read(MODEL).split("Self::RunPlayStep { step_kind, .. }", 1)
        self.assertEqual(len(payload), 2, "the payload class must come from the step kind")
        self.assertTrue(payload[1].lstrip().startswith("=> step_kind.action_class()"))

    def test_a_missed_step_is_recorded_rather_than_dropped(self) -> None:
        self.assertIn("skip_reason", self.sql)
        for reason in ("window_closed", "no_eligible_recipients", "anchor_withdrawn"):
            self.assertIn(f"'{reason}'", self.sql)
        # A reason on an unsettled step would describe a step that is still
        # running, so the database refuses it.
        self.assertIn("CHECK (skip_reason IS NULL OR settled_at IS NOT NULL)", self.sql)
        reasons = set(
            re.findall(r'Self::\w+ => "([a-z_]+)"', self.domain.split("impl StepSkipReason", 1)[1])
        )
        stored = re.search(r"skip_reason IS NULL OR skip_reason IN \((.*?)\)", self.sql, re.DOTALL)
        self.assertIsNotNone(stored)
        self.assertEqual(set(re.findall(r"'([a-z_]+)'", stored.group(1))), reasons)

    def test_a_late_step_is_never_delivered_late(self) -> None:
        # Order is the rule: withdrawal beats everything, expiry beats sending.
        rule = self.domain.split("pub fn evaluate_play", 1)[1].split("\n}", 1)[0]
        withdrawn = rule.index("AnchorWithdrawn")
        expired = rule.index("WindowClosed")
        sends = rule.index("PlayDecision::RunStep")
        self.assertLess(withdrawn, expired, "a cancelled show must not be promoted")
        self.assertLess(expired, sends, "a step outside its window must not be sent")

    def test_a_fan_is_reached_by_a_step_once_and_the_database_enforces_it(self) -> None:
        self.assertIn("UNIQUE (workspace_id, step_id, fan_id)", self.sql)
        # And a play exists once per anchor for ever, not once per anchor at a
        # time: a second play after the first completed would re-run a campaign
        # against fans who already had it.
        self.assertIn("UNIQUE (workspace_id, play_kind, anchor_kind, anchor_id)", self.sql)

    def test_a_send_is_idempotent_on_the_fan_with_no_time_component(self) -> None:
        key = re.search(
            r'action_idempotency_key: format!\(\s*"([^"]+)"', read(CANDIDATE)
        )
        self.assertIsNotNone(key)
        self.assertEqual(key.group(1), "action:play-step:{}:{}:{}")
        self.assertNotIn("cooldown_window", read(CANDIDATE))

    def test_eligibility_and_the_step_ceiling_read_committed_work(self) -> None:
        # Reading only the delivered table re-offers the fan whose action is
        # still pending every cycle: the play makes no progress, and stalls
        # entirely the moment a step needs approval.
        audience = self.infra.split("const PLAY_AUDIENCE_SQL", 1)[1].split('"#;', 1)[0]
        self.assertIn("viryaos_autopilot_actions", audience)
        self.assertIn("'play.step.run'", audience)
        self.assertIn("status <> 'cancelled'", audience)
        steps = self.infra.split("load_play_snapshots_impl", 1)[1]
        self.assertIn("AS recipients_emitted", steps)
        self.assertIn("viryaos_autopilot_actions", steps.split("AS recipients_emitted", 1)[0])

    def test_only_the_announce_step_accepts_interest_instead_of_attendance(self) -> None:
        # Thanking somebody for coming who did not come is a worse message than
        # sending nothing at all.
        audience = self.infra.split("const PLAY_AUDIENCE_SQL", 1)[1].split('"#;', 1)[0]
        interest = audience.index("event_interests")
        gate = audience.index("open_step.step_kind = 'announce_ask'")
        self.assertLess(gate, interest, "interest only qualifies for the announce ask")

    def test_consent_is_checked_when_the_message_is_sent(self) -> None:
        # Time passes between deciding and sending, and the one thing that must
        # not survive that gap is a message to somebody who withdrew consent.
        dispatch = self.infra.split("pub(super) async fn execute_play_step", 1)[1]
        self.assertIn("ensure_marketing_eligible", dispatch)
        self.assertIn("step.settled_at IS NULL", dispatch)
        self.assertIn("play.state = 'running'", dispatch)
        self.assertIn("event.status = 'published'", dispatch)

    def test_the_send_is_external_work_behind_a_named_capability(self) -> None:
        execution = read(EXECUTION)
        self.assertIn("AutopilotActionPayload::RunPlayStep { .. }", execution)
        self.assertIn('AutopilotActionPayload::RunPlayStep { .. } => "play.step"', execution)
        self.assertIn('"viryaos.play.step_requested" => "play.step"', execution)
        requires = execution.split("fn payload_requires_executor", 1)[1].split("\n}", 1)[0]
        self.assertIn("RunPlayStep", requires)
        self.assertIn("`play.step`", read(CONTRACT))
        self.assertIn("run_play_step", read(OPENAPI))

    def test_the_agent_can_start_advance_settle_and_finish_a_play(self) -> None:
        # Without any one of these the loop is not closed: no start and nothing
        # ever runs, no settle and an omission is invisible, no completion and
        # a finished campaign is read for ever.
        ports = read(PORTS)
        for method in (
            "load_play_anchors",
            "start_play",
            "load_play_snapshots",
            "settle_play_step",
            "complete_play",
        ):
            self.assertIn(f"async fn {method}(", ports)
        evaluate = read(EVALUATE)
        self.assertIn("AutopilotContext::Plays =>", evaluate)
        self.assertIn("plays_started", evaluate)
        self.assertIn("play_steps_skipped", evaluate)
        self.assertIn("plays_completed", evaluate)

    def test_a_play_is_only_completed_once_no_step_is_still_open(self) -> None:
        completion = self.infra.split("complete_play_impl", 1)[1]
        self.assertIn("state = 'completed'", completion)
        self.assertIn("settled_at IS NULL", completion)
        self.assertIn("NOT EXISTS", completion)

    def test_the_schedule_is_stored_rather_than_recomputed(self) -> None:
        # A play whose windows moved when the offsets in the code changed would
        # reschedule a campaign that is already running.
        self.assertIn("due_at timestamptz NOT NULL", self.sql)
        self.assertIn("expires_at timestamptz NOT NULL CHECK (expires_at > due_at)", self.sql)
        self.assertIn("pub fn step_schedule", self.domain)


if __name__ == "__main__":
    unittest.main()
