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


if __name__ == "__main__":
    unittest.main()
