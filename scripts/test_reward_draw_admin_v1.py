from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]


class RewardDrawAdminContracts(unittest.TestCase):
    def test_routes_are_admin_only_and_additive(self):
        router = (ROOT / "crates/crowdrelay-api/src/lib.rs").read_text()
        openapi = (ROOT / "openapi/openapi.yaml").read_text()
        self.assertIn('"/v1/admin/reward-draws"', router)
        self.assertIn('"/v1/admin/reward-draws/{draw_id}/delete"', router)
        self.assertNotIn('"/v1/staff/reward-draws/{draw_id}/delete"', router)
        self.assertIn("/admin/reward-draws/{draw_id}/delete:", openapi)
        self.assertIn("Fails closed once any run, winner or Proof-of-Fair receipt exists.", openapi)

    def test_delete_fails_closed_after_any_durable_draw_history(self):
        source = (ROOT / "crates/crowdrelay-api/src/commerce/campaigns.rs").read_text()
        delete = source.split("async fn delete_reward_draw_inner", 1)[1]
        delete = delete.split("async fn load_reward_fulfillments", 1)[0]
        self.assertIn('matches!(status.as_str(), "draft" | "scheduled" | "cancelled")', delete)
        self.assertIn("reward_draw_runs", delete)
        self.assertIn("reward_draw_winners", delete)
        self.assertIn("reward_draw_proofs", delete)
        self.assertIn("if durable_history.0 || durable_history.1 || durable_history.2", delete)
        self.assertIn("DELETE FROM reward_draws", delete)
        self.assertIn("reward_draw.deleted", delete)
        self.assertNotIn("DELETE FROM events", delete)
        self.assertNotIn("DELETE FROM admission_pools", delete)
        self.assertNotIn("DELETE FROM external_proof_batches", delete)
        self.assertNotIn("DELETE FROM reward_draw_proofs", delete)


    def test_management_dates_are_serialized_as_rfc3339_strings(self):
        source = (ROOT / "crates/crowdrelay-api/src/commerce.rs").read_text()
        campaign = source.split("pub struct RewardCampaignView", 1)[1].split("pub struct RewardDrawAdminView", 1)[0]
        draw = source.split("pub struct RewardDrawAdminView", 1)[1].split("pub struct DeletedRewardDrawView", 1)[0]
        for view in [campaign, draw]:
            self.assertGreaterEqual(view.count('#[serde(with = "time::serde::rfc3339")]'), 3)
            self.assertIn('#[serde(with = "time::serde::rfc3339::option")]', view)

    def test_management_list_exposes_why_delete_is_locked(self):
        source = (ROOT / "crates/crowdrelay-api/src/commerce/campaigns.rs").read_text()
        listing = source.split("async fn load_reward_draws", 1)[1]
        listing = listing.split("async fn delete_reward_draw_inner", 1)[0]
        for token in ["run_count", "selected_winners", "proof_count", "can_delete"]:
            self.assertIn(token, listing)
        self.assertIn("LEFT JOIN events", listing)


if __name__ == "__main__":
    unittest.main()
