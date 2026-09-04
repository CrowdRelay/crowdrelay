from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]


def text(path: str) -> str:
    return (ROOT / path).read_text()


class AutopilotMeasurementContract(unittest.TestCase):
    def test_db_enum_parser_and_rust_serializer_stay_aligned(self) -> None:
        migration = text("migrations/0054_grassroots_distribution.sql")
        # The measurement kinds moved out of `ports.rs` into their own module
        # when the signed/level distinction was added. This check follows the
        # vocabulary rather than the filename — it caught the move, which is
        # what it is for.
        ports = text("crates/crowdrelay-application/src/autopilot/measurement_ports.rs")
        support = text("crates/crowdrelay-infra/src/autopilot/support.rs")
        check = re.search(
            r"viryaos_autopilot_measurements_measurement_kind_check CHECK \(measurement_kind IN \((.*?)\)\)",
            migration,
            re.S,
        )
        self.assertIsNotNone(check)
        db_kinds = set(re.findall(r"'([a-z0-9_]+)'", check.group(1)))
        rust_kinds = set(re.findall(r'"([a-z0-9_]+)"', ports)) & db_kinds
        parsed_kinds = set(re.findall(r'"([a-z0-9_]+)" =>', support)) & db_kinds
        self.assertEqual(db_kinds, rust_kinds)
        self.assertEqual(db_kinds, parsed_kinds)

    def test_show_growth_measurements_have_real_durable_observers(self) -> None:
        execution = text("crates/crowdrelay-infra/src/autopilot/execution.rs")
        measurement = text("crates/crowdrelay-infra/src/autopilot/measurement.rs")
        runtime = text("crates/crowdrelay-infra/src/autopilot/runtime.rs")
        migration = text("migrations/0063_viryaos_show_growth_measurement_signals.sql")
        show_growth = text(
            "crates/crowdrelay-infra/src/autopilot/operations/show_growth_execution.rs"
        )
        for kind in (
            "ShowGrowthSurfaceClicks7d",
            "ShowGrowthAttributedTicketOrders7d",
            "GrassrootsActivationReplies14d",
        ):
            self.assertIn(kind, execution)
            self.assertIn(kind, measurement)
        self.assertIn("reply_recorded_at", migration)
        self.assertIn('get("reply_received")', runtime)
        self.assertIn("reply_received_semantics", show_growth)
        self.assertIn("reply_recorded_at >= $3", measurement)
        self.assertNotIn("status IN ('completed','introduced','sent','delivered')", measurement)

    def test_claim_quarantines_unknown_kind_before_commit(self) -> None:
        measurement = text("crates/crowdrelay-infra/src/autopilot/measurement.rs")
        parse_pos = measurement.index("match claimed_measurement(row)")
        quarantine_pos = measurement.index("unsupported_measurement_kind")
        commit_pos = measurement.index("transaction.commit().await", parse_pos)
        self.assertLess(parse_pos, commit_pos)
        self.assertLess(quarantine_pos, commit_pos)

    def test_multi_metric_guardrail_counts_distinct_actions(self) -> None:
        measurement = text("crates/crowdrelay-infra/src/autopilot/measurement.rs")
        self.assertIn("SELECT DISTINCT ON (outcome.action_id)", measurement)
        self.assertIn("latest_per_action", measurement)


if __name__ == "__main__":
    unittest.main()
