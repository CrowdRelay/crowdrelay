import unittest
from pathlib import Path
from rust_source_tree import read_rust_module

ROOT = Path(__file__).resolve().parents[1]


class OpsWatchdogContract(unittest.TestCase):
    def test_watchdog_is_native_deduplicated_and_provider_neutral(self):
        worker = (ROOT / "crates/crowdrelay-worker/src/ops_watchdog.rs").read_text()
        migration = (ROOT / "migrations/0046_viryaos_ops_watchdog.sql").read_text()
        main = (ROOT / "crates/crowdrelay-worker/src/main.rs").read_text()
        manifest_path = ROOT / "n8n/viryaos-executor-manifest.tsv"
        if not manifest_path.exists():
            self.skipTest(f"{manifest_path.relative_to(ROOT)} is a private n8n file and is not tracked in git")
        manifest = manifest_path.read_text()
        self.assertIn("viryaos_ops_alert_state", migration)
        self.assertIn("viryaos.ops.status_changed", worker)
        self.assertIn("ALERT_REPEAT_AFTER", worker)
        self.assertIn('"recovered"', worker)
        self.assertIn("OpsWatchdogWorker::new", main)
        self.assertIn("viryaos.ops.status_changed\tops.alert\t1\toperator_notification", manifest)
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
        latest = max(
            int(path.name.split("_", 1)[0])
            for path in (ROOT / "migrations").glob("[0-9][0-9][0-9][0-9]_*.sql")
        )
        self.assertIn(f"SCHEMA_VERSION: u32 = {latest}", meta)
        self.assertIn("schema_version: crate::meta::SCHEMA_VERSION", ops)


if __name__ == "__main__":
    unittest.main()
