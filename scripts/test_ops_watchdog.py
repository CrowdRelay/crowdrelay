import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


class OpsWatchdogContract(unittest.TestCase):
    def test_watchdog_is_native_deduplicated_and_provider_neutral(self):
        worker = (ROOT / "crates/crowdrelay-worker/src/ops_watchdog.rs").read_text()
        migration = (ROOT / "migrations/0046_viryaos_ops_watchdog.sql").read_text()
        main = (ROOT / "crates/crowdrelay-worker/src/main.rs").read_text()
        manifest = (ROOT / "n8n/viryaos-executor-manifest.tsv").read_text()
        self.assertIn("viryaos_ops_alert_state", migration)
        self.assertIn("viryaos.ops.status_changed", worker)
        self.assertIn("ALERT_REPEAT_AFTER", worker)
        self.assertIn('"recovered"', worker)
        self.assertIn("OpsWatchdogWorker::new", main)
        self.assertIn("viryaos.ops.status_changed\tops.alert\t1\toperator_notification", manifest)
        self.assertNotIn("retry_dead_outbox", worker)
        self.assertNotIn("retry_dead_delivery", worker)

    def test_watchdog_health_is_exposed_by_the_first_party_ops_summary(self):
        ops = (ROOT / "crates/crowdrelay-api/src/ops.rs").read_text()
        watchdog = (ROOT / "crates/crowdrelay-api/src/ops_summary.rs").read_text()
        self.assertIn("watchdog: WatchdogSummary", ops)
        self.assertIn("crate::ops_summary::load_watchdog_summary", ops)
        self.assertIn("FROM viryaos_ops_alert_state", watchdog)
        self.assertIn("active_alerts", watchdog)
        self.assertIn("critical_alerts", watchdog)

    def test_latest_schema_is_46(self):
        meta = (ROOT / "crates/crowdrelay-api/src/meta.rs").read_text()
        ops = (ROOT / "crates/crowdrelay-api/src/ops.rs").read_text()
        self.assertIn("SCHEMA_VERSION: u32 = 46", meta)
        self.assertIn("schema_version: 46", ops)


if __name__ == "__main__":
    unittest.main()
