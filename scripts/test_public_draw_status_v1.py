from pathlib import Path
import unittest
import yaml

ROOT = Path(__file__).resolve().parents[1]


class PublicDrawStatusContracts(unittest.TestCase):
    def test_public_status_route_is_additive_and_documented(self):
        router = (ROOT / "crates/crowdrelay-api/src/lib.rs").read_text()
        spec_text = (ROOT / "openapi/openapi.yaml").read_text()
        spec = yaml.safe_load(spec_text)
        self.assertIn('"/v1/public/proofs/draws/{draw_slug}/status"', router)
        self.assertIn("/public/proofs/draws/{draw_slug}/status", spec["paths"])
        self.assertIn("/public/proofs/draws/{draw_slug}", spec["paths"])
        self.assertIn("PUBLIC_DRAW_STATUS_CACHE", router.replace("lib.rs", "") if False else (ROOT / "crates/crowdrelay-api/src/proofs.rs").read_text())
        self.assertIn("max-age=5, s-maxage=10, must-revalidate", (ROOT / "crates/crowdrelay-api/src/proofs.rs").read_text())

    def test_status_exposes_only_lifecycle_and_proof_availability(self):
        source = (ROOT / "crates/crowdrelay-api/src/proofs.rs").read_text()
        status = source.split("async fn load_draw_status", 1)[1]
        status = status.split("async fn load_draw_proof", 1)[0]
        for token in [
            'schema: "crowdrelay/draw-status/v1"',
            "draw_slug",
            "draw_name",
            "status",
            "draw_at",
            "completed_at",
            "proof_available",
            "reward_draw_proofs",
        ]:
            self.assertIn(token, status)
        for private_token in ["normalized_email", "display_name", "fan_id", "ticket_order"]:
            self.assertNotIn(private_token, status)

    def test_missing_draw_is_not_confused_with_missing_receipt(self):
        source = (ROOT / "crates/crowdrelay-api/src/proofs.rs").read_text()
        status = source.split("async fn load_draw_status", 1)[1]
        status = status.split("async fn load_draw_proof", 1)[0]
        self.assertIn("fetch_optional", status)
        self.assertIn("ok_or(ProofError::NotFound)", status)
        self.assertIn("WHERE draw.workspace_id = $1 AND draw.slug = $2", status)


if __name__ == "__main__":
    unittest.main()
