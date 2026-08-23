"""Contract tests for the outreach-supply context.

This context exists because Phase 9 built a screening pipeline that nothing
ever fed. Candidate ingestion, dedupe, refusal and promotion all worked, and
`viryaos_outreach_targets` still held zero rows in production, because
discovery was inbound only: something outside the agent had to decide to look.
That made an empty pitcher a stable state rather than a problem, which is the
difference between an autonomous system and a well-tested set of rules.

The properties pinned here are the ones whose absence would quietly restore
that state, or turn the fix into a crawler:

- the request reaches nobody and buys nothing, so it stays first-party;
- it holds when the queue is waiting on a human, instead of growing it;
- it stops asking a dry source;
- the database, the Rust enum and the published contract still agree.

As the newest context migration, this file also owns the enum/constraint
equality claim.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
MIGRATION = ROOT / "migrations/0083_viryaos_outreach_supply.sql"
DOMAIN = ROOT / "crates/crowdrelay-domain/src/target_discovery.rs"
MODEL = ROOT / "crates/crowdrelay-application/src/autopilot/model.rs"
VALIDATION = ROOT / "crates/crowdrelay-api/src/autopilot/validation.rs"
MAPPING = ROOT / "crates/crowdrelay-infra/src/autopilot/mapping.rs"
EXECUTION = ROOT / "crates/crowdrelay-infra/src/autopilot/execution.rs"
ACTIONS = ROOT / "crates/crowdrelay-infra/src/autopilot/actions.rs"
LOADER = ROOT / "crates/crowdrelay-infra/src/autopilot/operations/discovery.rs"
CANDIDATE = (
    ROOT / "crates/crowdrelay-application/src/autopilot/evaluate/outreach_supply.rs"
)
OPENAPI = ROOT / "openapi/openapi.yaml"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def strip_sql_comments(sql: str) -> str:
    return "\n".join(line.split("--", 1)[0] for line in sql.splitlines())


class OutreachSupplyContract(unittest.TestCase):
    def setUp(self) -> None:
        self.migration = read(MIGRATION)
        self.domain = read(DOMAIN)

    def contexts(self) -> set[str]:
        model = read(MODEL)
        block = model.split("impl AutopilotContext", 1)[1].split(
            "/// Typed bounded-context", 1
        )[0]
        return set(re.findall(r'Self::\w+ => "([a-z0-9_]+)"', block))

    def test_every_context_check_constraint_matches_the_rust_enum(self) -> None:
        # This is the newest context migration, so it owns the equality claim.
        constraints = re.findall(
            r"ADD CONSTRAINT viryaos_autopilot_\w+_context_check CHECK \(context IN \((.*?)\)\)",
            self.migration,
            re.DOTALL,
        )
        self.assertEqual(
            len(constraints), 3, "policies, decisions and actions must all be updated"
        )
        for constraint in constraints:
            allowed = set(re.findall(r"'([a-z0-9_]+)'", constraint))
            self.assertEqual(
                allowed,
                self.contexts(),
                "database context constraint drifted from AutopilotContext",
            )

    def test_the_context_stores_nothing_of_its_own(self) -> None:
        # Supply is counted from the target and candidate tables that already
        # own those facts. A supply table would be a second, stale copy.
        self.assertNotIn("CREATE TABLE", strip_sql_comments(self.migration))

    def test_a_workspace_created_later_also_gets_the_context(self) -> None:
        # The backfill covers today's workspaces and the trigger covers
        # tomorrow's. Updating only one works until the next workspace exists.
        self.assertIn("SELECT workspace.id, 'outreach_supply', 2", self.migration)
        trigger = self.migration.split(
            "CREATE OR REPLACE FUNCTION viryaos_provision_autopilot_policies", 1
        )[1]
        self.assertIn("'outreach_supply', 2", trigger)
        provisioned = set(re.findall(r"NEW\.id, '([a-z_]+)'", trigger))
        self.assertEqual(
            provisioned,
            self.contexts(),
            "the provisioning trigger drifted from AutopilotContext",
        )

    def test_the_context_arrives_disabled_and_tightly_quota_limited(self) -> None:
        provisioning = self.migration.split(
            "INSERT INTO viryaos_autopilot_policies", 1
        )[1]
        columns = re.search(r"^\s*\(([^)]*)\)", provisioning)
        self.assertIsNotNone(columns)
        self.assertEqual(
            {column.strip() for column in columns.group(1).split(",")},
            {"workspace_id", "context", "max_actions_24h"},
            "a new context must inherit the disabled/observe defaults",
        )
        quota = re.search(r"'outreach_supply',\s*(\d+)", provisioning)
        self.assertIsNotNone(quota)
        self.assertLessEqual(
            int(quota.group(1)),
            4,
            "a sweep is a daily job; a large quota turns a cooldown bug into a crawl",
        )

    def test_the_context_is_reachable_from_every_parse_surface(self) -> None:
        self.assertIn(
            '"outreach_supply" => AutopilotContext::OutreachSupply', read(MAPPING)
        )
        self.assertIn(
            '"outreach_supply" => Some(AutopilotContext::OutreachSupply)',
            read(VALIDATION),
        )
        openapi = read(OPENAPI)
        enum_line = (
            openapi.split("AutopilotContext:", 1)[1]
            .split("enum: [", 1)[1]
            .split("]", 1)[0]
        )
        self.assertIn("outreach_supply", enum_line)

    def test_the_sweep_request_reaches_nobody_and_buys_nothing(self) -> None:
        # The whole reason this can run unattended: it reads published data.
        # Promoting it to an outward class would put a read behind the
        # third-party ceiling and silently stop the pipeline again.
        model = read(MODEL)
        first_party = model.split("Self::RequestBeaconDiscovery { .. }", 1)[1].split(
            "=> {", 1
        )[0]
        self.assertIn("RequestOutreachDiscovery", first_party)

    def test_the_capability_is_named_identically_on_both_gates(self) -> None:
        # The pre-claim check and the emission check must agree, or the action
        # is parked under one name and gated under another.
        execution = read(EXECUTION)
        self.assertIn(
            'AutopilotActionPayload::RequestOutreachDiscovery { .. } => "outreach.discovery"',
            execution,
        )
        self.assertIn(
            '"viryaos.outreach.discovery_requested" => "outreach.discovery"',
            execution,
        )

    def test_the_emitted_contract_forbids_inventing_a_contact(self) -> None:
        # The adapter is told the rules the ingest path will apply anyway. An
        # inferred address is a bounce at best and a burned curator at worst.
        emission = read(ACTIONS).split("viryaos.outreach.discovery_requested", 1)[1][
            :4000
        ]
        self.assertIn("never_infer_or_pattern_guess_an_address", emission)
        self.assertIn("send_the_verbatim_published_evidence", emission)
        self.assertIn("never_submit_through_a_channel_that_sells_placement", emission)
        self.assertIn("/v1/admin/autopilot/outreach/candidates", emission)

    def test_executors_report_on_the_internal_surface(self) -> None:
        # Handing an adapter the admin key so it can post playlist contacts
        # blurs the two authority surfaces the route map keeps apart.
        routing = read(ROOT / "crates/crowdrelay-api/src/routing.rs")
        self.assertIn("/v1/internal/autopilot/outreach/candidates", routing)
        api = read(ROOT / "crates/crowdrelay-api/src/autopilot/target_discovery.rs")
        internal = api.split("pub async fn ingest_outreach_candidates_internal", 1)[1]
        self.assertIn("commerce_authorized", internal.split("\n}", 1)[0])

    def test_an_empty_batch_is_reportable_on_the_internal_route(self) -> None:
        # A sweep that found nothing admissible must be able to say so, or the
        # barren back-off can never fire and the agent asks a dead source for
        # ever. The admin route keeps refusing empty batches: an operator who
        # imports nothing has made a mistake.
        api = read(ROOT / "crates/crowdrelay-api/src/autopilot/target_discovery.rs")
        internal = api.split("pub async fn ingest_outreach_candidates_internal", 1)[1].split(
            "\npub async fn ingest_outreach_candidates(", 1
        )[0]
        self.assertNotIn("candidates.is_empty()", internal)
        admin = api.split("\npub async fn ingest_outreach_candidates(", 1)[1]
        self.assertIn("request.candidates.is_empty()", admin.split("\n}", 1)[0])

    def test_an_unanswered_sweep_is_read_from_the_ingestion_not_the_candidates(
        self,
    ) -> None:
        # Zero candidate rows means both "found nothing" and "never reported".
        # Only the operator-action ledger separates them.
        loader = read(LOADER).split("load_outreach_supply_snapshot", 1)[1][:4000]
        self.assertIn("ingest_autopilot_outreach_candidates", loader)
        self.assertIn("operator_actions", loader)

    def test_a_full_human_queue_is_not_answered_with_more_candidates(self) -> None:
        rule = self.domain.split("pub fn evaluate_outreach_supply", 1)[1]
        self.assertIn("AwaitingRouteConfirmation", rule)
        self.assertIn(
            "candidates_waiting_on_a_human_are_not_answered_with_more_candidates",
            self.domain,
        )

    def test_a_dry_source_stops_being_asked_before_anything_else_is_considered(
        self,
    ) -> None:
        rule = self.domain.split("pub fn evaluate_outreach_supply", 1)[1]
        exhausted = rule.index("SourceExhausted")
        for later in ("SupplyIsAdequate", "CooldownActive"):
            self.assertLess(
                exhausted,
                rule.index(later),
                "exhaustion must outrank every other reason to keep sweeping",
            )
        self.assertIn(
            "exhaustion_outranks_every_other_reason_to_keep_sweeping", self.domain
        )

    def test_an_adapter_that_never_reported_is_not_mistaken_for_a_dry_source(
        self,
    ) -> None:
        # A sweep that produced no candidates at all is an integration failure
        # the operator must see, not evidence that the source is empty. Counting
        # it as barren would make one broken workflow disable discovery.
        loader = read(LOADER).split("load_outreach_supply_snapshot", 1)[1][:4000]
        self.assertIn("arrived = 0 OR survived > 0", loader)

    def test_the_sweep_window_is_bounded_per_sweep(self) -> None:
        # Without the window every older sweep is credited with every later
        # candidate, so a single good sweep hides an arbitrarily long dry run.
        loader = read(LOADER).split("load_outreach_supply_snapshot", 1)[1][:4000]
        self.assertIn("window_ends_at", loader)
        self.assertIn("lead(action.created_at)", loader)

    def test_the_example_sweep_never_infers_a_contact(self) -> None:
        # The published workflow is the thing an operator actually imports, so
        # the rule that keeps the supply clean has to survive in it too.
        sweep = read(ROOT / "n8n/examples/autopilot-outreach-discovery.example.json")
        self.assertIn("viryaos.outreach.discovery_requested", sweep)
        self.assertIn("route_is_published", sweep)
        self.assertIn("evidence", sweep)
        # The base URL already carries /v1, so the path is appended without it.
        self.assertIn("/internal/autopilot/outreach/candidates", sweep)
        self.assertNotIn("/v1/internal/autopilot/outreach/candidates", sweep)
        # An empty sweep must still be reported: silence is read as an
        # unanswered request and keeps the agent asking for ever, so the
        # extractor must always emit exactly one batch item.
        self.assertIn("Always one item, even with nothing found", sweep)
        for secret in ("Bearer sk-", "xoxb-", "hooks.slack.com/services/T"):
            self.assertNotIn(secret, sweep, "a literal secret leaked into the example")

    def test_the_domain_holds_no_provider_or_sql_concept(self) -> None:
        for forbidden in ("sqlx", "reqwest", "SELECT ", "INSERT "):
            self.assertNotIn(
                forbidden, self.domain, f"domain module leaked {forbidden!r}"
            )

    def test_the_decision_is_keyed_on_the_sweep_it_reacts_to(self) -> None:
        # Keying on the clock would re-ask every cycle; keying on the observed
        # supply and the last sweep makes two cycles that see the same starved
        # pipeline one decision.
        candidate = read(CANDIDATE)
        for key in ("decision_key", "action_idempotency_key"):
            block = candidate.split(f"{key}: format!(", 1)[1].split("),", 1)[0]
            self.assertIn("pitchable_targets", block)
            self.assertIn("last_sweep", block)
            self.assertNotIn("now", block)


if __name__ == "__main__":
    unittest.main()
