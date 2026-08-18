from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]

class RockSolidPublicReadPath(unittest.TestCase):
    def test_city_snapshot_and_stale_fallback(self):
        source = (ROOT / "crates/crowdrelay-api/src/acquisition.rs").read_text()
        api_contract = ((ROOT / "crates/crowdrelay-api/src/lib.rs").read_text() + (ROOT / "crates/crowdrelay-api/src/routing.rs").read_text())
        self.assertIn("CITY_SNAPSHOT_MAX_AGE", source)
        self.assertIn("city refresh failed; serving previous snapshot", source)
        self.assertIn("stale-if-error=86400", source)
        self.assertIn(
            '"public, max-age=60, stale-while-revalidate=600, stale-if-error=86400"',
            source,
        )
        self.assertNotIn(
            '"public, max-age=60, stale-while-revalidate=600"',
            source,
        )
        self.assertNotIn(".list_cities\n        .execute(\n            state.acquisition.workspace_id", source)

    def test_pool_and_health_do_not_amplify_transients(self):
        pool = (ROOT / "crates/crowdrelay-infra/src/database.rs").read_text()
        compose = (ROOT / "compose.production.yaml").read_text()
        self.assertIn(".min_connections(1)", pool)
        self.assertIn(".max_lifetime(Some(Duration::from_secs(30 * 60)))", pool)
        self.assertIn("/v1/health/live", compose)

    def test_public_events_isolate_provider_poison_and_match_domain_url_contract(self):
        repository = (ROOT / "crates/crowdrelay-infra/src/events.rs").read_text()
        worker = (ROOT / "crates/crowdrelay-worker/src/event_sync.rs").read_text()
        loader = repository[
            repository.index("async fn load_published_events_inner"):
            repository.index("async fn persist_event_action_inner")
        ]
        self.assertIn("skipping invalid published event row", loader)
        self.assertNotIn(".collect::<Result<Vec<_>, _>>()", loader)
        self.assertIn("fn valid_public_https_url", worker)
        self.assertIn('url.scheme() == "https"', worker)
        self.assertIn("url.host_str().is_some()", worker)
        self.assertIn("url.username().is_empty()", worker)
        self.assertIn("url.password().is_none()", worker)
        self.assertIn("url.fragment().is_none()", worker)
        self.assertNotIn("fn valid_http_url", worker)

    def test_external_production_smoke_includes_synesthesia_without_retrying_alert_post(self):
        smoke = (ROOT / ".github/workflows/production-smoke.yml").read_text()
        probe_script = (ROOT / "scripts/production-smoke.sh").read_text()
        self.assertIn("SYNESTHESIA_BASE_URL", smoke)
        self.assertIn("require_200 synesthesia_home", probe_script)
        self.assertIn("require_200 synesthesia_boot_art", probe_script)
        self.assertIn("--connect-timeout 4", probe_script)
        self.assertIn("--max-time 10", probe_script)
        self.assertIn("--retry 1", probe_script)
        self.assertNotIn("schedule:", smoke)
        self.assertIn("./scripts/production-smoke.sh", smoke)
        alert = smoke.split("- name: Alert Discord on failure", 1)[1]
        self.assertNotIn("--retry 2", alert)
        self.assertNotIn("--retry-all-errors", alert)


if __name__ == "__main__":
    unittest.main()
