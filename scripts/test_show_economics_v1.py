"""Contract tests for show economics — predicted cost against settled cost.

Phase 7 built a cost model good enough to refuse a gig with, and nothing ever
checked it. A rate that was wrong stayed wrong and kept deciding shows on a
number nobody had tested.

The properties pinned here are the ones whose absence would turn the loop into
a model marking its own work:

- the prediction is frozen before the show, with the rates it used, and written
  once;
- a settlement with no prediction behind it is refused, never backfilled;
- the first account of what happened stands; a second does not replace it;
- an unsettled show carries no verdict, and an unscoreable one carries a reason;
- only a drifting verdict may point an operator at a line to change;
- the implied road rate is evidence, never applied.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
MIGRATION = ROOT / "migrations/0090_viryaos_show_cost_ledger.sql"
DOMAIN = ROOT / "crates/crowdrelay-domain/src/show_settlement.rs"
PORTS = ROOT / "crates/crowdrelay-application/src/autopilot/control/show_cost_ports.rs"
INFRA = ROOT / "crates/crowdrelay-infra/src/autopilot/show_cost.rs"
API = ROOT / "crates/crowdrelay-api/src/autopilot/show_cost.rs"
ROUTING = ROOT / "crates/crowdrelay-api/src/routing.rs"
OPENAPI = ROOT / "openapi/openapi.yaml"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def strip_sql_comments(sql: str) -> str:
    return "\n".join(line.split("--", 1)[0] for line in sql.splitlines())


class ShowEconomicsContract(unittest.TestCase):
    def setUp(self) -> None:
        self.sql = strip_sql_comments(read(MIGRATION))
        self.domain = read(DOMAIN)
        self.infra = read(INFRA)

    def lines(self) -> set[str]:
        block = self.domain.split("impl CostLine", 1)[1].split("\n    }", 1)[0]
        return set(re.findall(r'Self::\w+ => "([a-z_]+)"', block))

    def gaps(self) -> set[str]:
        block = self.domain.split("impl SettlementGap", 1)[1].split("\n    }", 1)[0]
        return set(re.findall(r'Self::\w+ => "([a-z_]+)"', block))

    # --- the order the two writes must happen in ------------------------

    def test_the_prediction_is_frozen_with_the_rates_it_used(self) -> None:
        # Without the rates, an operator reading a variance a year later cannot
        # tell whether the model was wrong or merely old.
        self.assertIn("tour_policy_snapshot jsonb NOT NULL", self.sql)
        freeze = self.infra.split("fn freeze_show_cost_prediction", 1)[1]
        self.assertIn("tour_policy_snapshot", freeze.split("RETURNING id", 1)[0])

    def test_a_prediction_is_written_once_and_never_revised(self) -> None:
        # Re-freezing after the show would let the goalposts move.
        self.assertIn("UNIQUE (workspace_id, event_id)", self.sql)
        freeze = self.infra.split("fn freeze_show_cost_prediction", 1)[1]
        self.assertIn("ON CONFLICT (workspace_id, event_id) DO NOTHING", freeze)
        self.assertIn("replayed: inserted.is_none()", freeze)

    def test_a_settlement_without_a_prediction_is_refused(self) -> None:
        settle = self.infra.split("fn settle_show_cost", 1)[1]
        refusal = settle.index("RepositoryError::NotFound")
        update = settle.index("UPDATE viryaos_show_cost_ledger")
        self.assertLess(
            refusal,
            update,
            "a model cannot be scored against a show it was never asked about",
        )
        self.assertIn("NoPrediction", self.domain)

    def test_the_first_account_of_what_happened_stands(self) -> None:
        settle = self.infra.split("fn settle_show_cost", 1)[1]
        self.assertIn("frozen.settled_at.is_some()", settle)
        self.assertIn("replayed: true", settle)
        self.assertIn("AND settled_at IS NULL", settle)

    # --- what the database refuses --------------------------------------

    def test_half_a_prediction_is_not_a_prediction(self) -> None:
        self.assertIn(
            "CHECK ((prediction_missing_input IS NULL) = (predicted_total_cost_minor IS NOT NULL))",
            self.sql,
        )
        self.assertIn(
            "CHECK ((settled_at IS NULL) = (settled_total_cost_minor IS NULL))", self.sql
        )

    def test_a_verdict_exists_exactly_when_a_settlement_does(self) -> None:
        self.assertIn(
            "CHECK ((accuracy IS NOT NULL) = (settled_at IS NOT NULL))", self.sql
        )

    def test_only_a_drifting_verdict_may_name_a_line_to_change(self) -> None:
        self.assertIn(
            "CHECK (worst_line IS NULL OR accuracy IS NOT DISTINCT FROM 'drifting')",
            self.sql,
        )
        self.assertIn(
            "CHECK (worst_line_delta_minor IS NULL OR worst_line IS NOT NULL)", self.sql
        )

    def test_an_unscoreable_show_names_its_reason(self) -> None:
        self.assertIn(
            "CHECK ((accuracy IS NOT DISTINCT FROM 'insufficient') = (accuracy_reason IS NOT NULL))",
            self.sql,
        )

    def test_the_verdict_constraints_are_null_safe(self) -> None:
        # A CHECK whose expression evaluates to NULL passes. On a row that is
        # still only a prediction, a plain `=` would wave through a verdict with
        # nothing behind it.
        for constraint in ("reason_matches_accuracy", "worst_line_requires_drift"):
            clause = self.sql.split(f"viryaos_show_cost_ledger_{constraint}", 1)[1].split(
                "\n", 2
            )[1]
            self.assertIn("IS NOT DISTINCT FROM", clause)

    def test_every_stored_value_matches_the_rust_enum(self) -> None:
        stored_lines = re.search(
            r"worst_line IS NULL OR worst_line IN \((.*?)\)", self.sql, re.DOTALL
        )
        self.assertIsNotNone(stored_lines)
        self.assertEqual(set(re.findall(r"'([a-z_]+)'", stored_lines.group(1))), self.lines())
        stored_gaps = re.search(
            r"accuracy_reason IS NULL OR accuracy_reason IN \((.*?)\)", self.sql, re.DOTALL
        )
        self.assertIsNotNone(stored_gaps)
        self.assertEqual(set(re.findall(r"'([a-z_]+)'", stored_gaps.group(1))), self.gaps())

    # --- what the rule refuses to invent --------------------------------

    def test_the_worst_line_is_the_one_that_moved_the_most_money(self) -> None:
        rule = self.domain.split("pub fn assess_model_accuracy", 1)[1]
        self.assertIn("max_by_key", rule)
        self.assertIn("delta_minor().unsigned_abs()", rule)
        self.assertIn("minimum_material_minor", rule)

    def test_a_line_predicted_at_nothing_yields_no_percentage(self) -> None:
        self.assertIn("fn relative_variance", self.domain)
        self.assertIn(
            "a_line_predicted_at_nothing_yields_no_percentage", self.domain
        )

    def test_the_implied_road_rate_is_evidence_and_never_applied(self) -> None:
        self.assertIn("pub fn implied_transport_rate_minor_per_100km", self.domain)
        # Nothing writes the tour economics rates from a settlement.
        self.assertNotIn("UPDATE viryaos_tour_economics", self.infra)

    def test_the_remedy_travels_with_the_finding(self) -> None:
        self.assertIn("pub const fn remedy(self)", self.domain)
        self.assertIn("worst_line_remedy", read(PORTS))
        self.assertIn("worst.map(CostLine::remedy)", self.infra)

    # --- the boundary ---------------------------------------------------

    def test_the_two_writes_and_the_read_are_published(self) -> None:
        routing = read(ROUTING)
        for route in (
            '"/v1/admin/events/{event_id}/show-cost/prediction"',
            '"/v1/admin/events/{event_id}/show-cost/settlement"',
            '"/v1/admin/autopilot/show-economics"',
        ):
            self.assertIn(route, routing)
        openapi = read(OPENAPI)
        for path in (
            "/admin/events/{event_id}/show-cost/prediction:",
            "/admin/events/{event_id}/show-cost/settlement:",
            "/admin/autopilot/show-economics:",
        ):
            self.assertIn(path, openapi)
        published = re.search(r"ShowCostLine:.*?enum: \[(.*?)\]", openapi, re.DOTALL)
        self.assertIsNotNone(published)
        self.assertEqual(
            {value.strip() for value in published.group(1).split(",")}, self.lines()
        )

    def test_money_and_distance_are_bounded_at_the_boundary(self) -> None:
        # A settlement two orders of magnitude out would drag the model's
        # calibration off on its own.
        api = read(API)
        self.assertIn("MAX_SHOW_MONEY_MINOR", api)
        self.assertIn("MAX_SHOW_DISTANCE_KM", api)
        self.assertIn("fn plausible_money", api)


if __name__ == "__main__":
    unittest.main()
