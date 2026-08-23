"""Contract tests for the external growth-metrics layer.

These assert the properties that are cheap to break silently: that a new
detector cannot arrive already switched on, that the database still agrees with
the Rust context enum, and that derived movement is never stored as a second
truth beside the observations it came from.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
MIGRATION = ROOT / "migrations/0073_viryaos_growth_metrics.sql"
DOMAIN = ROOT / "crates/crowdrelay-domain/src/growth_metrics.rs"
MODEL = ROOT / "crates/crowdrelay-application/src/autopilot/model.rs"
INFRA = ROOT / "crates/crowdrelay-infra/src/autopilot/growth_metrics.rs"
WORKER = ROOT / "crates/crowdrelay-worker/src/autopilot.rs"
RETENTION = ROOT / "crates/crowdrelay-worker/src/retention/steps.rs"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def strip_sql_comments(sql: str) -> str:
    """Drops `--` comments so prose about a column is not mistaken for one."""
    return "\n".join(line.split("--", 1)[0] for line in sql.splitlines())


class GrowthMetricsContract(unittest.TestCase):
    def setUp(self) -> None:
        self.migration = read(MIGRATION)

    def contexts(self) -> set[str]:
        model = read(MODEL)
        block = model.split("impl AutopilotContext", 1)[1].split(
            "/// Typed bounded-context", 1
        )[0]
        return set(re.findall(r'Self::\w+ => "([a-z0-9_]+)"', block))

    def test_migration_creates_the_series_and_point_tables(self) -> None:
        self.assertIn("CREATE TABLE viryaos_growth_metric_series", self.migration)
        self.assertIn("CREATE TABLE viryaos_growth_metric_points", self.migration)

    def test_observations_are_unique_per_capture_time(self) -> None:
        # Without this, a provider retry double-counts and every derived delta
        # behind it is wrong.
        self.assertIn(
            "UNIQUE (workspace_id, series_id, captured_at)",
            self.migration,
        )

    def test_derived_movement_is_never_stored(self) -> None:
        # Deltas, velocity and baselines are computed from the observations on
        # every read. Storing them would let a backfill leave a stale derived
        # row that no longer matches the timeline it claims to describe.
        series = strip_sql_comments(
            self.migration.split("CREATE TABLE viryaos_growth_metric_series", 1)[
                1
            ].split(");", 1)[0]
        )
        points = strip_sql_comments(
            self.migration.split("CREATE TABLE viryaos_growth_metric_points", 1)[
                1
            ].split(");", 1)[0]
        )
        for column in ("delta_", "velocity", "baseline", "anomaly", "trend"):
            self.assertNotIn(column, series, f"series stores derived column {column}")
            self.assertNotIn(column, points, f"points store derived column {column}")

    def test_the_new_context_is_provisioned_disabled_and_observing(self) -> None:
        # Every provisioning statement must supply only the quota, so the
        # table defaults (enabled=false, autonomy_level='observe') apply. A
        # detector that arrives enabled would act before it was ever watched.
        for statement in re.findall(
            r"INSERT INTO viryaos_autopilot_policies \(([^)]*)\)", self.migration
        ):
            columns = {column.strip() for column in statement.split(",")}
            self.assertEqual(
                columns,
                {"workspace_id", "context", "max_actions_24h"},
                f"provisioning statement sets more than the quota: {columns}",
            )
        self.assertNotIn("autonomy_level", self.migration)
        self.assertNotIn("enabled", self.migration)

    def test_growth_metrics_is_provisioned_for_existing_and_future_workspaces(
        self,
    ) -> None:
        self.assertIn("SELECT id, 'growth_metrics'", self.migration)
        provisioning = self.migration.split(
            "CREATE OR REPLACE FUNCTION viryaos_provision_autopilot_policies", 1
        )[1]
        self.assertIn("'growth_metrics'", provisioning)

    def test_every_context_check_constraint_is_a_subset_of_the_rust_enum(self) -> None:
        # This migration is history: a later one may legitimately add contexts
        # it never knew about, so it cannot be pinned to equality. What it must
        # never contain is a context the enum does not have — that direction is
        # drift, and it is the direction that breaks writes at runtime. The
        # newest context migration owns the equality assertion.
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
            self.assertIn("growth_metrics", allowed)
            self.assertLessEqual(
                allowed,
                contexts,
                "database context constraint drifted from AutopilotContext",
            )

    def test_the_domain_holds_no_provider_or_sql_concept(self) -> None:
        domain = read(DOMAIN)
        for forbidden in ("sqlx", "http", "reqwest", "SELECT ", "INSERT "):
            self.assertNotIn(
                forbidden, domain, f"domain module leaked {forbidden!r}"
            )

    def test_series_identity_is_null_safe(self) -> None:
        # Most series have no subject. Under default NULL semantics two
        # workspace-level series for the same metric would not conflict, so
        # every upsert would open a second timeline for the same number.
        self.assertIn(
            "UNIQUE NULLS NOT DISTINCT (workspace_id, platform, metric_key, subject_kind, subject_id)",
            self.migration,
        )

    def test_first_party_capture_is_bucketed_and_never_overwrites(self) -> None:
        # The worker cycle is far shorter than an hour. Without a bucket the
        # window would describe our polling rate rather than the business.
        # Counted against the number of capture statements rather than pinned
        # to a fixed number, so adding a series cannot silently ship one that
        # skips the bucket or overwrites history.
        infra = read(INFRA)
        capture = infra.split("impl AutopilotFirstPartyGrowthMetrics", 1)[1]
        statements = capture.count("INSERT INTO viryaos_growth_metric_points")
        self.assertGreaterEqual(statements, 2)
        self.assertEqual(
            capture.count("date_trunc('hour', $2::timestamptz)"),
            statements,
            "every first-party capture statement must bucket capture time",
        )
        self.assertEqual(
            capture.count(
                "ON CONFLICT (workspace_id, series_id, captured_at) DO NOTHING"
            ),
            statements,
            "first-party capture must never overwrite an existing observation",
        )

    def test_event_series_are_retired_rather_than_left_to_rot(self) -> None:
        # A series for a show that finished months ago stops receiving points
        # and would then be reported as a dead feed forever.
        infra = read(INFRA)
        self.assertIn("SET active = false", infra)
        self.assertIn("sync_event_ticketing_series", infra)

    def test_observations_are_recorded_before_the_evaluator_runs(self) -> None:
        worker = read(WORKER)
        capture = worker.find("materialize_first_party_growth_metrics")
        evaluate = worker.find("EvaluateAutopilot::new")
        self.assertNotEqual(capture, -1, "worker never captures first-party metrics")
        self.assertLess(
            capture, evaluate, "a cycle must reason about the newest evidence it can"
        )

    def test_observations_outside_the_derived_window_are_reclaimed(self) -> None:
        retention = read(RETENTION)
        self.assertIn("delete_expired_growth_metric_points", retention)
        self.assertIn("viryaos_growth_metric_points", retention)

    def test_absent_windows_are_optional_rather_than_zero(self) -> None:
        # "We have no observation that old" and "the number did not move" are
        # different facts, and only one of them is worth acting on.
        domain = read(DOMAIN)
        trend = domain.split("pub struct MetricTrend {", 1)[1].split("\n}", 1)[0]
        for field in ("delta_24h", "delta_7d", "delta_28d"):
            self.assertRegex(trend, rf"{field}: Option<i64>")


if __name__ == "__main__":
    unittest.main()
