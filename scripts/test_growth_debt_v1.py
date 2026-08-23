"""Contract tests for the growth-debt context.

Growth debt is the one detector with no storage of its own, so the properties
worth pinning are the ones that would let it quietly grow some: that it stays
derived from the tables that already own the facts, that it arrives disabled
like every other context, that the database and the Rust enum still agree, and
that its two refusals — expired deadlines and empty denominators — survive a
well-meaning edit.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
MIGRATION = ROOT / "migrations/0074_viryaos_growth_debt.sql"
DOMAIN = ROOT / "crates/crowdrelay-domain/src/growth_debt.rs"
MODEL = ROOT / "crates/crowdrelay-application/src/autopilot/model.rs"
VALIDATION = ROOT / "crates/crowdrelay-api/src/autopilot/validation.rs"
MAPPING = ROOT / "crates/crowdrelay-infra/src/autopilot/mapping.rs"
LOADER = ROOT / "crates/crowdrelay-infra/src/autopilot/operations/growth_debt.rs"
CANDIDATE = (
    ROOT / "crates/crowdrelay-application/src/autopilot/evaluate/growth_debt.rs"
)
ACTIONS = ROOT / "crates/crowdrelay-infra/src/autopilot/actions.rs"
CHIEF = ROOT / "crates/crowdrelay-infra/src/autopilot/operations/chief.rs"
OPENAPI = ROOT / "openapi/openapi.yaml"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def strip_sql_comments(sql: str) -> str:
    """Drops `--` comments so prose about a table is not mistaken for one."""
    return "\n".join(line.split("--", 1)[0] for line in sql.splitlines())


class GrowthDebtContract(unittest.TestCase):
    def setUp(self) -> None:
        self.migration = read(MIGRATION)
        self.domain = read(DOMAIN)

    def contexts(self) -> set[str]:
        model = read(MODEL)
        block = model.split("impl AutopilotContext", 1)[1].split(
            "/// Typed bounded-context", 1
        )[0]
        return set(re.findall(r'Self::\w+ => "([a-z0-9_]+)"', block))

    def test_the_context_stores_nothing_of_its_own(self) -> None:
        # A debt table would be a second, immediately stale copy of facts the
        # booking, outreach, event and release tables already own.
        self.assertNotIn("CREATE TABLE", strip_sql_comments(self.migration))

    def test_the_new_context_is_provisioned_disabled_and_observing(self) -> None:
        provisioning = self.migration.split(
            "CREATE OR REPLACE FUNCTION viryaos_provision_autopilot_policies", 1
        )[1]
        self.assertIn("'growth_debt', 10", provisioning)
        columns = re.search(
            r"INSERT INTO viryaos_autopilot_policies \(([^)]*)\)", provisioning
        )
        self.assertIsNotNone(columns)
        self.assertEqual(
            {column.strip() for column in columns.group(1).split(",")},
            {"workspace_id", "context", "max_actions_24h"},
            "a new context must inherit the disabled/observe defaults",
        )

    def test_growth_debt_is_provisioned_for_existing_and_future_workspaces(self) -> None:
        self.assertIn("SELECT id, 'growth_debt'", self.migration)

    def test_every_context_check_constraint_matches_the_rust_enum(self) -> None:
        # This is the newest context migration, so it owns the equality claim.
        contexts = self.contexts()
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
                contexts,
                "database context constraint drifted from AutopilotContext",
            )

    def test_the_context_is_reachable_from_every_parse_surface(self) -> None:
        self.assertIn('"growth_debt" => AutopilotContext::GrowthDebt', read(MAPPING))
        self.assertIn(
            '"growth_debt" => Some(AutopilotContext::GrowthDebt)', read(VALIDATION)
        )
        openapi = read(OPENAPI)
        enum_line = openapi.split("AutopilotContext:", 1)[1].split("enum: [", 1)[1].split(
            "]", 1
        )[0]
        self.assertIn("growth_debt", enum_line)
        # The context path parameter must reference that one enum rather than
        # inlining a copy: the inline copy silently fell two contexts behind and
        # published a contract that rejected values the API accepts.
        parameter = openapi.split("AutopilotContextPath:", 1)[1].split("AutopilotActionId:", 1)[0]
        self.assertIn("$ref: '#/components/schemas/AutopilotContext'", parameter)
        self.assertNotIn("enum:", parameter)

    def test_the_domain_holds_no_provider_or_sql_concept(self) -> None:
        for forbidden in ("sqlx", "http", "reqwest", "SELECT ", "INSERT "):
            self.assertNotIn(
                forbidden, self.domain, f"domain module leaked {forbidden!r}"
            )

    def test_debt_past_its_deadline_is_never_raised(self) -> None:
        # An event that already played cannot be promoted. Reporting it would be
        # accurate and useless, and it would crowd out payable debt.
        rule = self.domain.split("pub fn evaluate_growth_debt", 1)[1]
        self.assertIn("is_deadline_bound()", rule)
        self.assertIn("hours < 0", rule)
        self.assertIn("deadline_bound_debt_without_a_date_is_dropped", self.domain)
        self.assertIn("debt_whose_deadline_has_passed_is_dropped", self.domain)

    def test_debt_is_never_claimed_from_an_empty_denominator(self) -> None:
        rule = self.domain.split("pub fn evaluate_growth_debt", 1)[1]
        self.assertIn("observation.tracked_items == 0", rule)
        self.assertIn(
            "nothing_tracked_is_never_reported_as_everything_neglected", self.domain
        )

    def test_the_share_ratio_is_computed_wide_enough_to_survive_an_aggregate(
        self,
    ) -> None:
        # u32 saturating arithmetic here silently reports a fully neglected
        # subject as ~0% outstanding once the item count passes ~429k.
        rule = self.domain.split("pub fn evaluate_growth_debt", 1)[1]
        self.assertIn("u64::from(outstanding)", rule)
        self.assertIn("priority_and_confidence_stay_inside_their_ranges", self.domain)

    def test_hygiene_debt_cannot_outrank_debt_with_an_outcome_at_stake(self) -> None:
        # The same value-tier ordering as growth_metrics, deliberately shared:
        # one ordering decides what outranks what across both detectors.
        self.assertIn("growth_metrics::MetricValueTier", self.domain)
        self.assertIn("MetricValueTier::Downstream", self.domain)
        self.assertIn(
            "downstream_debt_outranks_hygiene_debt_at_the_same_overdue_ratio",
            self.domain,
        )

    def test_the_rule_reports_evidence_and_never_a_cause(self) -> None:
        reasons = self.domain.split("pub const fn reason", 1)[1].split("}", 1)[0]
        for forbidden in ("because", "caused", "due to"):
            self.assertNotIn(forbidden, reasons.lower())

    def test_the_cooldown_is_read_back_per_subject_and_debt_kind(self) -> None:
        # One event can owe both skipped levers and a stalled release plan.
        # Grouping the last signal by subject alone would let raising one
        # silence the other for a fortnight, which is why the decision kind
        # carries the debt kind rather than being one shared string.
        self.assertIn("pub const fn decision_kind", self.domain)
        loader = read(LOADER)
        self.assertIn("GROUP BY subject_id, decision_kind", loader)
        self.assertIn("decision_kind: item.kind.decision_kind()", read(CANDIDATE))

    def test_the_loader_reports_facts_and_leaves_the_arithmetic_to_the_domain(
        self,
    ) -> None:
        # A ratio or a priority computed in SQL is a second copy of the rule
        # that drifts from the first one silently.
        loader = read(LOADER)
        for forbidden in ("basis_points", "priority", "confidence"):
            self.assertNotIn(forbidden, loader.split('r#"', 1)[1])

    def test_the_idle_clock_falls_back_rather_than_capping(self) -> None:
        # `created_at` inside the GREATEST makes it a ceiling on idleness, not a
        # floor: a row created today carrying an outreach timestamp from last
        # year reads as touched today, and no relationship is ever quiet. Found
        # against a real database — a target 200 days idle reported 0 hours.
        loader = read(LOADER)
        quiet = loader.split("WITH quiet_relationships AS", 1)[1].split(
            "skipped_levers AS", 1
        )[0]
        for branch in quiet.split("UNION ALL"):
            self.assertIn("COALESCE(", branch)
            greatest = branch.split("COALESCE(", 1)[1].split("target.created_at", 1)[0]
            self.assertIn("GREATEST(", greatest)
            self.assertNotIn("created_at", greatest)

    def test_the_loader_only_reads(self) -> None:
        loader = read(LOADER)
        for forbidden in ("INSERT ", "UPDATE ", "DELETE "):
            self.assertNotIn(forbidden, loader)

    def test_deliberate_choices_are_not_counted_as_neglect(self) -> None:
        # A surface somebody skipped or retired is a decision, not debt.
        loader = read(LOADER)
        self.assertIn("surface.status <> 'skipped'", loader)
        self.assertIn("surface.status <> 'retired'", loader)

    def test_the_observation_query_is_bounded(self) -> None:
        loader = read(LOADER)
        self.assertIn("LIMIT $4", loader)
        self.assertIn("MAX_SNAPSHOTS_PER_CONTEXT", loader)

    def test_raising_debt_has_no_outward_side_effect(self) -> None:
        # Debt is an observation. Auto-sending outreach off the back of it would
        # move paid, contractual work behind an observation quota.
        actions = read(ACTIONS)
        arm = actions.split("AutopilotActionPayload::RaiseGrowthDebt { .. } => {", 1)[1]
        self.assertEqual(arm.split("}", 1)[0].strip().count("await"), 0)

    def test_growth_findings_reach_the_existing_operator_brief(self) -> None:
        # The chief-of-staff opportunity query filters by context. A detector
        # missing from that list produces decisions nobody ever sees, which is
        # indistinguishable from not having built it.
        chief = read(CHIEF)
        allow_list = chief.split("AND decision.context IN (", 1)[1].split(")", 1)[0]
        self.assertIn("'growth_debt'", allow_list)
        self.assertIn("'growth_metrics'", allow_list)

    def test_the_action_kind_fits_the_published_contract(self) -> None:
        # `action_kind` is a free-form bounded string in the contract, not an
        # enum, so the only thing to hold is the length bound.
        self.assertIn('Self::RaiseGrowthDebt { .. } => "growth.debt.raise"', read(MODEL))
        openapi = read(OPENAPI)
        self.assertIn("action_kind: { type: string, maxLength: 96 }", openapi)
        self.assertLessEqual(len("growth.debt.raise"), 96)


class ShowGrowthDoesTheWorkContract(unittest.TestCase):
    """The agent completes free work rather than filing it as a suggestion."""

    def setUp(self) -> None:
        self.domain = read(ROOT / "crates/crowdrelay-domain/src/show_growth.rs")
        self.execution = read(
            ROOT / "crates/crowdrelay-infra/src/autopilot/operations/show_growth_execution.rs"
        )

    def test_a_show_is_given_a_tracked_link_before_anything_is_shared(self) -> None:
        # Every lever hands somebody a URL. A URL nobody tracks turns all of
        # that work into an unmeasurable guess, and the honest-attribution
        # promise elsewhere in the system depends on the link existing.
        self.assertIn("CanonicalLinkSetup", self.domain)
        rule = self.domain.split("pub fn evaluate_show_growth", 1)[1]
        self.assertLess(
            rule.index("ShowGrowthLever::CanonicalLinkSetup"),
            rule.index("ShowGrowthLever::FreeListingSweep"),
            "the link must be set up before the first lever that shares it",
        )
        self.assertIn("a_show_gets_a_tracked_link_before_anything_is_shared", self.domain)

    def test_the_link_is_created_rather_than_described(self) -> None:
        # An instruction to an executor is a suggestion. This writes the row.
        self.assertIn("INSERT INTO smart_links", self.execution)
        self.assertIn("ensure_canonical_show_link", self.execution)

    def test_a_show_with_no_destination_gets_no_link(self) -> None:
        # A tracked route to nowhere is worse than an untracked route to the
        # right place.
        body = self.execution.split("async fn ensure_canonical_show_link", 1)[1].split(
            "\n#[allow", 1
        )[0]
        self.assertIn('filter(|url| url.starts_with("http"))', body)
        self.assertIn("RepositoryError::Conflict", body)

    def test_rerunning_the_setup_repairs_rather_than_duplicates(self) -> None:
        body = self.execution.split("async fn ensure_canonical_show_link", 1)[1].split(
            "\n#[allow", 1
        )[0]
        self.assertIn("ON CONFLICT (workspace_id, slug) DO UPDATE", body)
        self.assertIn("active = true", body)

    def test_a_release_is_given_a_tracked_link_too(self) -> None:
        # Same rule as a show: nothing is shared before there is a link that
        # can be counted.
        execution = read(ROOT / "crates/crowdrelay-infra/src/autopilot/operations/execution.rs")
        links = read(ROOT / "crates/crowdrelay-infra/src/autopilot/operations/release_links.rs")
        self.assertIn("ensure_release_tracked_link", links)
        self.assertIn("INSERT INTO smart_links", links)
        # Run for every milestone, not only the first: the first can fail on a
        # missing executor capability and the announcement must not then go out
        # untracked.
        milestone = execution.split("async fn execute_release_milestone", 1)[1].split(
            "\n    use crowdrelay_domain::release_autopilot", 1
        )[0]
        self.assertIn("ensure_release_tracked_link(tx", milestone)

    def test_two_releases_can_never_share_one_link(self) -> None:
        # A release key is free text and the slug is unique per workspace, so
        # sanitising is lossy in a way that matters: two keys reducing to the
        # same letters would make the second overwrite the first's destination.
        links = read(ROOT / "crates/crowdrelay-infra/src/autopilot/operations/release_links.rs")
        slug = links.split("fn release_link_slug", 1)[1].split("\n}", 1)[0]
        self.assertIn("key_digest(source_key)", slug)
        self.assertIn("bounded == source_key.to_ascii_lowercase()", slug)

    def test_a_release_with_no_listen_url_gets_no_link(self) -> None:
        links = read(ROOT / "crates/crowdrelay-infra/src/autopilot/operations/release_links.rs")
        body = links.split("async fn ensure_release_tracked_link", 1)[1].split(
            "\n}", 1
        )[0]
        self.assertIn('filter(|url| url.starts_with("http"))', body)

    def test_the_room_is_asked_to_follow_after_the_show(self) -> None:
        # Attendance is the strongest signal in the system and the ask is free.
        self.assertIn("PostShowFollowAsk", self.domain)
        self.assertIn("post_show_follow_ask_hours", self.domain)
        self.assertIn(
            "the_room_is_asked_to_follow_once_the_merch_window_closes", self.domain
        )
        # It must not collide with the merch message about the same night.
        self.assertIn("the_merch_window_still_wins_while_it_is_open", self.domain)
        self.assertIn("the_follow_window_may_not_close_before_the_merch_window", self.domain)

    def test_the_follow_ask_carries_the_links_it_is_asking_about(self) -> None:
        block = self.execution.split("ShowGrowthLever::PostShowFollowAsk => json!", 1)[1]
        self.assertIn("bandsintown_follow_url", block)
        self.assertIn("spotify_artist_url", block)
        self.assertIn("must_read_as_a_thank_you_first_and_an_ask_second", block)
        self.assertIn("do_not_ask_for_money_in_this_message", block)


if __name__ == "__main__":
    unittest.main()
