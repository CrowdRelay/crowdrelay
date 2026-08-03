from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]

class RockSolidPublicReadPath(unittest.TestCase):
    def test_city_snapshot_and_stale_fallback(self):
        source = (ROOT / "crates/crowdrelay-api/src/acquisition.rs").read_text()
        api_contract = (ROOT / "crates/crowdrelay-api/src/lib.rs").read_text()
        self.assertIn("CITY_SNAPSHOT_MAX_AGE", source)
        self.assertIn("city refresh failed; serving previous snapshot", source)
        self.assertIn("stale-if-error=86400", source)
        self.assertIn(
            '"public, max-age=60, stale-while-revalidate=600, stale-if-error=86400"',
            api_contract,
        )
        self.assertNotIn(
            '"public, max-age=60, stale-while-revalidate=600"',
            api_contract,
        )
        self.assertNotIn(".list_cities\n        .execute(\n            state.acquisition.workspace_id", source)

    def test_pool_and_health_do_not_amplify_transients(self):
        pool = (ROOT / "crates/crowdrelay-infra/src/database.rs").read_text()
        compose = (ROOT / "compose.production.yaml").read_text()
        self.assertIn(".min_connections(1)", pool)
        self.assertIn(".max_lifetime(Some(Duration::from_secs(30 * 60)))", pool)
        self.assertIn("/v1/health/live", compose)

if __name__ == "__main__":
    unittest.main()
