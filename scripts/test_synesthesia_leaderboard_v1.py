#!/usr/bin/env python3
"""Source contract for replayable Synesthesia attempts and public opt-in scores."""
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]


class SynesthesiaLeaderboardV1Contract(unittest.TestCase):
    def test_replays_get_independent_attempts(self):
        migration = (ROOT / "migrations/0044_synesthesia_leaderboard.sql").read_text()
        api = (ROOT / "crates/crowdrelay-api/src/synesthesia.rs").read_text()
        self.assertIn("ADD COLUMN attempt_id text NOT NULL DEFAULT 'legacy'", migration)
        self.assertIn("synesthesia_runs_attempt_uidx", migration)
        self.assertIn("workspace_id, campaign_slug, install_hash, attempt_id", migration)
        self.assertIn("attempt_id: Option<String>", api)
        self.assertIn("clean_attempt_id", api)
        self.assertIn("payload.client_total_elapsed_ms != recorded_elapsed_ms", api)

    def test_public_entries_do_not_expose_identity_material(self):
        api = (ROOT / "crates/crowdrelay-api/src/synesthesia.rs").read_text()
        start = api.index("struct LeaderboardEntryResponse")
        end = api.index("struct LeaderboardPublishResponse")
        public_entry = api[start:end]
        for forbidden in ("install_hash", "run_id", "fan_id", "email", "normalized_email"):
            self.assertNotIn(forbidden, public_entry)
        self.assertIn("display_name: String", public_entry)
        self.assertIn("elapsed_ms: i64", public_entry)

    def test_only_best_attempt_per_install_is_ranked(self):
        api = (ROOT / "crates/crowdrelay-api/src/synesthesia.rs").read_text()
        self.assertGreaterEqual(api.count("SELECT DISTINCT ON (run.install_hash)"), 2)
        self.assertIn("ROW_NUMBER() OVER (ORDER BY elapsed_ms, completed_at, id)", api)
        self.assertIn("pub async fn list_leaderboard", api)
        self.assertIn("pub async fn publish_leaderboard", api)

    def test_routes_openapi_and_meta_ship_together(self):
        routes = (ROOT / "crates/crowdrelay-api/src/routing.rs").read_text()
        spec = (ROOT / "openapi/openapi.yaml").read_text()
        meta = (ROOT / "crates/crowdrelay-api/src/meta.rs").read_text()
        self.assertIn('"/v1/public/synesthesia/leaderboard"', routes)
        self.assertIn('"/v1/public/synesthesia/runs/{run_id}/leaderboard"', routes)
        self.assertIn("/public/synesthesia/leaderboard:", spec)
        self.assertIn("/public/synesthesia/runs/{run_id}/leaderboard:", spec)
        self.assertIn("SynesthesiaLeaderboardResponse:", spec)
        self.assertIn("SynesthesiaLeaderboardPublishRequest:", spec)
        self.assertIn("SCHEMA_VERSION: u32 = 44", meta)
        self.assertIn('"synesthesia_leaderboard_v1"', meta)


if __name__ == "__main__":
    unittest.main()
