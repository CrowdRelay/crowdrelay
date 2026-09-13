import unittest
from pathlib import Path
from rust_source_tree import read_rust_module

ROOT = Path(__file__).resolve().parents[1]


class OpsWatchdogContract(unittest.TestCase):
    def test_watchdog_is_native_deduplicated_and_provider_neutral(self):
        worker = (ROOT / "crates/crowdrelay-worker/src/ops_watchdog.rs").read_text()
        migration = (ROOT / "migrations/0046_viryaos_ops_watchdog.sql").read_text()
        main = (ROOT / "crates/crowdrelay-worker/src/main.rs").read_text()
        self.assertIn("viryaos_ops_alert_state", migration)
        # The watchdog tracks alert state in the DB but no longer emits
        # outbox events — Discord spam was removed because it was not
        # actionable from there. Alert state is exposed via the ops API.
        self.assertIn("viryaos_ops_alert_state", worker)
        self.assertIn("ALERT_REPEAT_AFTER", worker)
        self.assertNotIn("crowdrelay.ops.status_changed", worker)
        # The rule is that the watchdog does not *emit*, not that it may not read.
        # This was `assertNotIn("outbox_events", worker)`, which is a proxy for
        # the rule and forbids more than the rule does: diagnosing why a delivery
        # was refused needs `outbox_events.event_type`, since `webhook_deliveries`
        # carries only the id. Reading a table in order to raise an alert is the
        # opposite of emitting an event from the watchdog.
        for write in ("INSERT INTO outbox_events", "UPDATE outbox_events",
                      "DELETE FROM outbox_events"):
            self.assertNotIn(write, worker)
        self.assertIn("OpsWatchdogWorker::new", main)
        self.assertNotIn("retry_dead_outbox", worker)
        self.assertNotIn("retry_dead_delivery", worker)

    def test_watchdog_health_is_exposed_by_the_first_party_ops_summary(self):
        ops = read_rust_module(ROOT, "crates/crowdrelay-api/src/ops.rs")
        watchdog = (ROOT / "crates/crowdrelay-api/src/ops_summary.rs").read_text()
        self.assertIn("watchdog: WatchdogSummary", ops)
        self.assertIn("crate::ops_summary::load_watchdog_summary", ops)
        self.assertIn("FROM viryaos_ops_alert_state", watchdog)
        self.assertIn("active_alerts", watchdog)
        self.assertIn("critical_alerts", watchdog)

    def test_public_schema_tracks_latest_migration(self):
        meta = (ROOT / "crates/crowdrelay-api/src/meta.rs").read_text()
        ops = read_rust_module(ROOT, "crates/crowdrelay-api/src/ops.rs")
        # SCHEMA_VERSION is auto-discovered by build.rs — verify the pattern.
        self.assertIn("CROWDRELAY_SCHEMA_VERSION", meta)
        self.assertTrue((ROOT / "crates/crowdrelay-api/build.rs").is_file())
        self.assertIn("schema_version: crate::meta::SCHEMA_VERSION", ops)


if __name__ == "__main__": unittest.main()
