import json
import re
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


class AutopilotGrowthExecutionContractTest(unittest.TestCase):
    def read(self, relative: str) -> str:
        return (ROOT / relative).read_text(encoding="utf-8")

    def workflow(self, relative: str) -> dict:
        return json.loads(self.read(relative))

    def node_names(self, workflow: dict) -> list[str]:
        return [node["name"] for node in workflow["nodes"]]

    def test_free_fan_push_is_a_real_first_party_action(self):
        source = self.read("crates/crowdrelay-domain/src/show_growth.rs")
        self.assertIn("Self::FreeFanChannelPush", source)
        self.assertRegex(
            source,
            r"(?s)is_first_party_campaign\(self\).*?Self::FreeFanChannelPush",
        )

    def test_free_fan_push_has_real_owned_growth_ctas(self):
        source = self.read(
            "crates/crowdrelay-infra/src/autopilot/operations/show_growth_execution.rs"
        )
        self.assertIn('"bandsintown_follow_url": "env:VIRYA_BANDSINTOWN_FOLLOW_URL"', source)
        self.assertIn('"spotify_artist_url": "env:VIRYA_SPOTIFY_ARTIST_URL"', source)
        self.assertIn('"spotify_playlist_url": "env:VIRYA_SPOTIFY_PLAYLIST_URL"', source)
        self.assertIn('"owned_email_delivered"', source)

    def test_playlist_release_seeder_is_gated_and_idempotent(self):
        source = self.read("migrations/0064_release_playlist_outreach.sql")
        for required in (
            "NEW.milestone <> 'start_press'",
            "target.target_kind = 'playlist'",
            "target.verified",
            "target.accepts_outreach",
            "NOT target.do_not_contact",
            "ON CONFLICT (workspace_id, source, target_id, subject_kind, subject_key)",
        ):
            self.assertIn(required, source)

    def test_outreach_executor_claims_before_side_effect_and_reports_after(self):
        workflow = self.workflow("n8n/examples/autopilot-outreach-executor.example.json")
        names = self.node_names(workflow)
        self.assertLess(names.index("Claim action once"), names.index("Send Gmail pitch"))
        self.assertLess(names.index("Claim action once"), names.index("Submit verified free form"))
        self.assertIn("Report provider receipt", names)
        serialized = self.read("n8n/examples/autopilot-outreach-executor.example.json")
        self.assertNotIn("6bbW0jOKAWJWm3h6CTWaAS", serialized)
        self.assertNotIn("virya.music/pl/epk/", serialized)
        self.assertIn("route.verified!==true", serialized)
        self.assertIn("route.free!==true", serialized)
        self.assertIn("route.requires_captcha===true", serialized)

    def test_reply_monitor_is_idempotent_and_closes_followups(self):
        workflow = self.workflow("n8n/examples/autopilot-outreach-reply-monitor.example.json")
        names = self.node_names(workflow)
        self.assertIn("Resolve CrowdRelay action", names)
        self.assertIn("Record reply and stop follow-ups", names)
        serialized = self.read("n8n/examples/autopilot-outreach-reply-monitor.example.json")
        self.assertIn("gmail-inbound:", serialized)
        self.assertIn("disposition:'received'", serialized)

    def test_free_fan_campaign_executor_is_end_to_end(self):
        workflow = self.workflow("n8n/examples/autopilot-free-fan-campaign.example.json")
        names = self.node_names(workflow)
        required = [
            "List campaigns",
            "Load delivery page",
            "Claim delivery once",
            "Send through canonical mailer",
            "Record delivery receipt",
            "Restore campaign context",
            "Check campaign progress",
            "Complete campaign",
        ]
        for name in required:
            self.assertIn(name, names)
        serialized = self.read("n8n/examples/autopilot-free-fan-campaign.example.json")
        self.assertIn("show.growth.free_fan_push.v1", serialized)
        self.assertIn("VIRYA_BANDSINTOWN_FOLLOW_URL", serialized)
        self.assertIn("VIRYA_SPOTIFY_ARTIST_URL", serialized)
        self.assertIn("VIRYA_SPOTIFY_PLAYLIST_URL", serialized)
        self.assertIn("Idempotency-Key", serialized)
        self.assertIn("/deliveries/", serialized)
        self.assertIn("/complete", serialized)

    def test_no_growth_workflow_contains_fake_stream_or_paid_placement_automation(self):
        paths = [
            "n8n/examples/autopilot-free-fan-campaign.example.json",
            "n8n/examples/autopilot-outreach-executor.example.json",
        ]
        banned = re.compile(
            r"(?i)(buy\s*streams|stream\s*bot|click\s*farm|fake\s*followers|guaranteed\s*playlist|paid\s*placement)"
        )
        for path in paths:
            self.assertIsNone(banned.search(self.read(path)), path)


if __name__ == "__main__":
    unittest.main()
