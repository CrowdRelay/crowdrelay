from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]


def text(path: str) -> str:
    return (ROOT / path).read_text()


class AutopilotMeasurementContract(unittest.TestCase):
    def test_db_enum_parser_and_rust_serializer_stay_aligned(self) -> None:
        # Every kinds migration rewrites the whole CHECK list, so the latest
        # file carrying the constraint is the vocabulary of record. Pinning
        # the original 0054 here would intersect the Rust side down to the
        # kinds that existed then and let a typo in a newer arm pass.
        kind_migrations = sorted(
            path
            for path in (ROOT / "migrations").glob("*.sql")
            if "ADD CONSTRAINT viryaos_autopilot_measurements_measurement_kind_check"
            in path.read_text()
        )
        self.assertTrue(kind_migrations)
        migration = kind_migrations[-1].read_text()
        # The measurement kinds moved out of `ports.rs` into their own module
        # when the signed/level distinction was added. This check follows the
        # vocabulary rather than the filename — it caught the move, which is
        # what it is for.
        ports = text("crates/crowdrelay-application/src/autopilot/measurement_ports.rs")
        support = text("crates/crowdrelay-infra/src/autopilot/support.rs")
        check = re.search(
            r"viryaos_autopilot_measurements_measurement_kind_check\s+CHECK\s*\(measurement_kind IN \((.*?)\)\)",
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
        # The observation arms moved into `measurement/observation.rs` when the
        # adapter crossed the size ratchet. Both files are read for the same
        # reason the first check follows the vocabulary rather than the
        # filename: what matters is that a scheduled kind has somewhere that
        # observes it, not which file that is.
        measurement = text("crates/crowdrelay-infra/src/autopilot/measurement.rs") + text(
            "crates/crowdrelay-infra/src/autopilot/measurement/observation.rs"
        )
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

    def test_named_beacon_outreach_closes_the_primary_fan_loop(self) -> None:
        migration = text("migrations/0393_beacon_growth_learning_loop.sql")
        beacon_execution = text(
            "crates/crowdrelay-infra/src/autopilot/execution_beacon.rs"
        )
        scheduling = text("crates/crowdrelay-infra/src/autopilot/execution.rs")
        attribution = text(
            "crates/crowdrelay-infra/src/autopilot/measurement/observation/attributed_fans.rs"
        )
        acquisition = text("crates/crowdrelay-infra/src/acquisition/acquisition_events.rs")
        observation = text(
            "crates/crowdrelay-infra/src/autopilot/measurement/observation/beacons.rs"
        )

        # A non-post outreach owns its redirect directly; the signup path must
        # prefer that owner over the publication-ledger fallback.
        self.assertIn("ADD COLUMN IF NOT EXISTS action_id uuid", migration)
        self.assertIn("action_id,\n            channel_source", beacon_execution)
        self.assertIn("COALESCE(link.action_id, post.action_id)", acquisition)

        # The canonical fan learner, not a Beacon-only vanity counter, is the
        # North-Star outcome. Y3 gives fast feedback; Y14 and Y30 mature it.
        for kind in (
            "IncrementalFanGrowth3d",
            "IncrementalFanGrowth14d",
            "DurableFanGrowth30d",
        ):
            self.assertIn(kind, scheduling)
        self.assertNotIn("BeaconOutreachFanAcquisition14d", scheduling)

        # The generic fan observer must treat a successfully delivered
        # action-owned redirect as a live acquisition surface.
        self.assertIn("FROM smart_links AS link", attribution)
        self.assertIn("action.id=link.action_id", attribution)
        self.assertIn("action.status='succeeded'", attribution)
        self.assertIn("action.finished_at IS NOT NULL", attribution)

        # Proximal Beacon signals remain exact and per-action: one reply and
        # distinct people, never event-wide traffic borrowed by every target.
        self.assertIn("BeaconOutreachReply14d", scheduling)
        self.assertIn("BeaconOutreachUniqueVisitors14d", scheduling)
        self.assertIn("COUNT(DISTINCT click.anonymous_visitor_id)", observation)
        self.assertIn("reply.action='record_autopilot_beacon_reply'", observation)
        self.assertIn("newer.action_kind='beacon.outreach.request'", observation)

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
