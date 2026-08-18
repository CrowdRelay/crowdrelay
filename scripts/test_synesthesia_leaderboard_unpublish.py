import pathlib
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]

def read(path: str) -> str:
    return (ROOT / path).read_text()

class SynesthesiaLeaderboardUnpublishContract(unittest.TestCase):
    def test_api_delegates_write_to_infra(self):
        api = read("crates/crowdrelay-api/src/fan_privacy.rs")
        routes = read("crates/crowdrelay-api/src/routing.rs")
        self.assertIn("pub async fn unpublish_synesthesia_leaderboard", api)
        self.assertIn(".unpublish_synesthesia_leaderboard(", api)
        self.assertNotIn("UPDATE synesthesia_runs", api)
        self.assertIn('"/v1/me/synesthesia/leaderboard"', routes)
        self.assertIn("delete(fan_privacy::unpublish_synesthesia_leaderboard)", routes)
        unpublish = api.split("pub async fn unpublish_synesthesia_leaderboard", 1)[1]
        self.assertIn("(CACHE_CONTROL, PRIVATE_NO_STORE)", unpublish)

    def test_infra_unpublishes_all_fan_runs_without_unlinking_history(self):
        infra = read("crates/crowdrelay-infra/src/fan_privacy.rs")
        block = infra.split("pub async fn unpublish_synesthesia_leaderboard", 1)[1].split("async fn lock_current_fan", 1)[0]
        self.assertIn("UPDATE synesthesia_runs", block)
        self.assertIn("leaderboard_name = NULL", block)
        self.assertIn("leaderboard_published_at = NULL", block)
        self.assertIn("fan_id = $2", block)
        self.assertIn("leaderboard_name IS NOT NULL", block)
        self.assertNotIn("fan_id = NULL", block)
        self.assertIn("synesthesia.leaderboard_unpublished", block)

    def test_endpoint_and_capability_are_public_contract(self):
        spec = read("openapi/openapi.yaml")
        meta = read("crates/crowdrelay-api/src/meta.rs")
        compatibility = read("integration/ecosystem/compatibility.json")
        self.assertIn("/me/synesthesia/leaderboard:", spec)
        self.assertIn("SynesthesiaLeaderboardUnpublishResponse", spec)
        self.assertIn('("synesthesia_leaderboard_unpublish_v1", true)', meta)
        self.assertIn('"synesthesia_leaderboard_unpublish_v1"', compatibility)

if __name__ == "__main__":
    unittest.main()
