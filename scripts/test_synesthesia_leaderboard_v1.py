#!/usr/bin/env python3
"""Source contract for replayable Synesthesia attempts and privacy-safe public scores."""
from pathlib import Path
from rust_source_tree import read_rust_module
import unittest

ROOT = Path(__file__).resolve().parents[1]


class SynesthesiaLeaderboardV1Contract(unittest.TestCase):
    def test_replays_get_independent_attempts(self):
        migration = (ROOT / "migrations/0044_synesthesia_leaderboard.sql").read_text()
        api = read_rust_module(ROOT, "crates/crowdrelay-api/src/synesthesia.rs")
        self.assertIn("ADD COLUMN attempt_id text NOT NULL DEFAULT 'legacy'", migration)
        self.assertIn("synesthesia_runs_attempt_uidx", migration)
        self.assertIn("workspace_id, campaign_slug, install_hash, attempt_id", migration)
        self.assertIn("attempt_id: Option<String>", api)
        self.assertIn("clean_attempt_id", api)
        self.assertIn("payload.client_total_elapsed_ms != recorded_elapsed_ms", api)

    def test_public_entries_do_not_expose_identity_material(self):
        api = (ROOT / "crates/crowdrelay-api/src/synesthesia/leaderboard.rs").read_text()
        start = api.index("struct LeaderboardEntryResponse")
        end = api.index("struct LeaderboardResponse")
        public_entry = api[start:end]
        for forbidden in ("install_hash", "run_id", "fan_id", "email", "normalized_email"):
            self.assertNotIn(forbidden, public_entry)
        self.assertIn("display_name: String", public_entry)
        self.assertIn("elapsed_ms: i64", public_entry)

    def test_publish_accepts_bounded_public_display_name_without_fan_identity(self):
        api = (ROOT / "crates/crowdrelay-api/src/synesthesia/leaderboard.rs").read_text()
        publish = api.split("pub async fn publish_leaderboard", 1)[1]
        self.assertIn("LeaderboardPublishRequest", api)
        self.assertIn("display_name: Option<String>", api)
        self.assertIn("normalize_leaderboard_name", publish)
        self.assertIn('Ok("anonymous".to_owned())', api)
        self.assertNotIn("fan.normalized_email", publish)
        self.assertNotIn("masked_email_alias", api)

    def test_only_best_attempt_per_install_is_ranked_and_indexed(self):
        api = (ROOT / "crates/crowdrelay-api/src/synesthesia/leaderboard.rs").read_text()
        migration = (ROOT / "migrations/0071_synesthesia_install_leaderboard_index.sql").read_text()
        # One entry per install, and that entry is the install's best attempt.
        # The board listing gets there with DISTINCT ON; the publish reply gets
        # there by counting distinct installs ahead of this one's best, because
        # ranking the whole board to read a single rank costs the size of the
        # board on every publish. Both must still key on `install_hash` and
        # order an install's attempts by elapsed time, so a fan with five tries
        # appears once, at their fastest.
        self.assertIn("SELECT DISTINCT ON (run.install_hash)", api)
        self.assertIn("ORDER BY run.install_hash, run.client_total_elapsed_ms, run.completed_at, run.id", api)
        self.assertIn("count(DISTINCT run.install_hash)", api)
        self.assertIn(
            "ORDER BY run.client_total_elapsed_ms, run.completed_at, run.id\n            LIMIT 1",
            api,
        )
        self.assertIn(
            "(run.client_total_elapsed_ms, run.completed_at, run.id)\n                      < (my_best.elapsed_ms, my_best.completed_at, my_best.id)",
            api,
        )
        self.assertIn("synesthesia_runs_leaderboard_best_idx", migration)
        self.assertIn("install_hash", migration)
        self.assertIn("DROP INDEX IF EXISTS synesthesia_runs_leaderboard_fan_best_idx", migration)

    def test_routes_openapi_and_meta_ship_together(self):
        # The public synesthesia routes moved behind the module gate inside
        # the synesthesia module; search both mount points.
        routes = (
            (ROOT / "crates/crowdrelay-api/src/routing.rs").read_text()
            + (ROOT / "crates/crowdrelay-api/src/synesthesia.rs").read_text()
        )
        spec = (ROOT / "openapi/openapi.yaml").read_text()
        meta = (ROOT / "crates/crowdrelay-api/src/meta.rs").read_text()
        self.assertIn('"/v1/public/synesthesia/leaderboard"', routes)
        self.assertIn('"/v1/public/synesthesia/runs/{run_id}/leaderboard"', routes)
        self.assertIn("/public/synesthesia/leaderboard:", spec)
        self.assertIn("/public/synesthesia/runs/{run_id}/leaderboard:", spec)
        self.assertIn("SynesthesiaLeaderboardResponse:", spec)
        self.assertIn("SynesthesiaLeaderboardPublishResponse:", spec)
        # SCHEMA_VERSION is auto-discovered by build.rs — verify the pattern.
        self.assertIn("CROWDRELAY_SCHEMA_VERSION", meta)
        self.assertTrue((ROOT / "crates/crowdrelay-api/build.rs").is_file())
        self.assertIn('"synesthesia_leaderboard_v1"', meta)


if __name__ == "__main__":
    unittest.main()
