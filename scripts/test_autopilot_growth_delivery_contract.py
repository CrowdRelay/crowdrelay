import json
import re
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


class AutopilotGrowthDeliveryContractTest(unittest.TestCase):
    def read(self, relative: str) -> str:
        return (ROOT / relative).read_text(encoding="utf-8")

    def workflow(self, relative: str) -> dict:
        return json.loads(self.read(relative))

    def test_playlist_migration_has_unique_version_and_safety_gates(self):
        source = self.read("migrations/0072_release_playlist_outreach.sql")
        for required in (
            "NEW.milestone <> 'start_press'",
            "target.target_kind = 'playlist'",
            "target.active",
            "target.verified",
            "target.accepts_outreach",
            "NOT target.do_not_contact",
            "ON CONFLICT (workspace_id, source, target_id, subject_kind, subject_key)",
        ):
            self.assertIn(required, source)

    def test_growth_delivery_supports_existing_provider_campaigns(self):
        workflow = self.workflow("n8n/examples/autopilot-growth-campaign-delivery.example.json")
        names = [node["name"] for node in workflow["nodes"]]
        for name in (
            "List campaigns",
            "Load consented delivery page",
            "Claim delivery once",
            "Send through canonical mailer",
            "Record delivery receipt",
            "Check campaign progress",
            "Complete campaign",
        ):
            self.assertIn(name, names)
        serialized = self.read("n8n/examples/autopilot-growth-campaign-delivery.example.json")
        for template in (
            "show.growth.free_fan_push.v1",
            "autopilot.spotify.follow.v1",
            "autopilot.bandsintown.follow.v1",
        ):
            self.assertIn(template, serialized)
        self.assertIn("Idempotency-Key", serialized)
        self.assertIn("retryOnFail", serialized)
        self.assertIn("VIRYA_MAIL_DELIVERY_URL", serialized)

    def test_delivery_claim_precedes_mailer_and_receipt_follows(self):
        names = [node["name"] for node in self.workflow("n8n/examples/autopilot-growth-campaign-delivery.example.json")["nodes"]]
        self.assertLess(names.index("Claim delivery once"), names.index("Send through canonical mailer"))
        self.assertLess(names.index("Send through canonical mailer"), names.index("Record delivery receipt"))
        self.assertLess(names.index("Record delivery receipt"), names.index("Check campaign progress"))

    def test_reconciler_closes_zero_pending_campaigns(self):
        workflow = self.workflow("n8n/examples/autopilot-growth-campaign-reconciler.example.json")
        names = [node["name"] for node in workflow["nodes"]]
        self.assertLess(names.index("Read campaign progress"), names.index("Reconcile campaign completion"))
        serialized = self.read("n8n/examples/autopilot-growth-campaign-reconciler.example.json")
        self.assertIn("pending_count", serialized)
        self.assertIn("/complete", serialized)

    def test_growth_delivery_has_no_artificial_growth_automation(self):
        paths = [
            "n8n/examples/autopilot-growth-campaign-delivery.example.json",
            "n8n/examples/autopilot-growth-campaign-reconciler.example.json",
        ]
        banned = re.compile(
            r"(?i)(buy\s*streams|stream\s*bot|click\s*farm|fake\s*followers|guaranteed\s*playlist|paid\s*placement)"
        )
        for path in paths:
            self.assertIsNone(banned.search(self.read(path)), path)


if __name__ == "__main__":
    unittest.main()
