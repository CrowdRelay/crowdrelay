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

    def test_free_fan_push_is_delivered_by_the_campaign_due_executor(self):
        """Growth campaigns ride the existing campaign delivery path.

        `execute_first_party_growth_campaign` schedules the campaign and emits
        `communication.campaign_due`, so the generic campaign executor already
        owns claim/send/receipt/complete. A second poller would mean two
        workers claiming the same deliveries, so none is shipped here and the
        contract is enforced on the producer instead.
        """
        source = self.read(
            "crates/crowdrelay-infra/src/autopilot/operations/show_growth_execution.rs"
        )
        self.assertIn("'communication.campaign_due'", source)
        self.assertIn("ShowGrowthLever::FreeFanChannelPush", source)
        # The CTAs travel in the campaign content, so any consumer of
        # campaign_due can render them without provider-specific wiring.
        self.assertIn('"growth_ctas"', source)

    def test_growth_cta_prefers_a_url_crowdrelay_already_holds(self):
        """An `env:` CTA is only as good as the executor's environment.

        Every CTA in this lever was an `env:` placeholder, so a deployment that
        had not defined those variables sent a follow-us email with no links --
        the one thing the lever exists to deliver, and a failure invisible until
        the first real send. The event's own listen URL is known here, so it
        travels as a real URL and only the provider-native follow/playlist links
        remain deferred.
        """
        source = self.read(
            "crates/crowdrelay-infra/src/autopilot/operations/show_growth_execution.rs"
        )
        self.assertIn("event.listen_url", source)
        # There are two growth_ctas blocks. The one in execute_show_growth's
        # constraints match is unreachable for this lever -- it is first-party,
        # so execution returns before that match. Only the campaign content
        # built in execute_first_party_growth_campaign reaches the executor, so
        # assert against that function specifically rather than the whole file.
        live = source.split("async fn execute_first_party_growth_campaign", 1)[1]
        self.assertRegex(
            live,
            r'(?s)"spotify_artist_url":\s*event\s*\.5\s*\.clone\(\)\s*'
            r'\.unwrap_or_else\(\|\|\s*"env:VIRYA_SPOTIFY_ARTIST_URL"',
        )

    def test_no_polling_campaign_worker_is_shipped(self):
        """A poller would need admin credentials and would double-claim.

        Campaign delivery is event-driven. Reintroducing a workflow that lists
        campaigns from the admin API puts a second claimant on the same
        deliveries and needs an admin token in n8n.
        """
        for retired in (
            "n8n/examples/autopilot-free-fan-campaign.example.json",
            "n8n/examples/autopilot-growth-campaign-delivery.example.json",
            "n8n/examples/autopilot-growth-campaign-reconciler.example.json",
        ):
            self.assertFalse(
                (ROOT / retired).exists(),
                f"{retired} duplicates the campaign_due delivery path",
            )
        # Creating a campaign from a provider executor is fine; what must not
        # come back is a scheduled trigger that lists campaigns and delivers
        # them behind the event-driven executor's back.
        for path in sorted((ROOT / "n8n/examples").glob("*.example.json")):
            workflow = json.loads(path.read_text(encoding="utf-8"))
            polls = any(
                node["type"].endswith("scheduleTrigger") for node in workflow["nodes"]
            )
            if not polls:
                continue
            for node in workflow["nodes"]:
                url = node.get("parameters", {}).get("url", "")
                method = node.get("parameters", {}).get("method", "GET")
                self.assertFalse(
                    method == "GET" and "communications/campaigns" in url,
                    f"{path.name} polls the campaign list on a schedule",
                )

    def test_bandsintown_executor_hands_off_to_real_campaign_delivery(self):
        workflow = self.workflow("n8n/examples/autopilot-bandsintown-growth.example.json")
        names = self.node_names(workflow)
        for name in (
            "Claim action once",
            "Fail-closed claim gate",
            "Read Bandsintown artist state",
            "Read Bandsintown upcoming events",
            "Create consented Bandsintown CTA campaign",
            "Campaign creation gate",
            "Schedule Bandsintown CTA campaign",
            "Report Bandsintown growth receipt",
        ):
            self.assertIn(name, names)
        serialized = self.read("n8n/examples/autopilot-bandsintown-growth.example.json")
        # The campaign must use a template the generic growth delivery worker
        # actually sends; the consented audience comes from the provisioned
        # growth segment rather than an ad-hoc segment write.
        self.assertIn("autopilot.bandsintown.follow.v1", serialized)
        self.assertIn("VIRYA_BANDSINTOWN_GROWTH_SEGMENT_SLUG", serialized)
        self.assertIn("CROWDRELAY_ADMIN_TOKEN", serialized)

    def test_growth_delivery_progress_is_observable_from_the_control_plane(self):
        router = self.read("crates/crowdrelay-api/src/control_plane.rs")
        self.assertIn('"/v1/control-plane/autopilot/growth"', router)
        self.assertIn("get(crate::autopilot::growth)", router)
        # The private tunnel is fail-closed: an un-allowlisted route is
        # unreachable in production even though the router accepts it.
        caddy = self.read("deploy/area-management.Caddyfile")
        self.assertIn("/v1/control-plane/autopilot/growth", caddy)

    def test_growth_read_model_counts_the_ledger_and_leaks_no_contact_data(self):
        adapter = self.read("crates/crowdrelay-infra/src/autopilot/growth.rs")
        # Progress must come from the delivery ledger. The campaign summary
        # columns are only written at completion, so a stalled campaign would
        # otherwise be indistinguishable from a finished one.
        self.assertIn("communication_campaign_deliveries", adapter)
        self.assertIn("communication_campaign_recipients", adapter)
        for status in ("'delivered'", "'failed'", "'claimed'"):
            self.assertIn(status, adapter)
        # Outreach targets carry contact_email. The Control Plane is a platform
        # surface, so this read model must stay aggregate-only.
        for forbidden in ("contact_email", "display_name", "fan_id", "email"):
            self.assertNotIn(forbidden, adapter, forbidden)

    def test_no_growth_workflow_contains_fake_stream_or_paid_placement_automation(self):
        paths = [
            "n8n/examples/autopilot-outreach-executor.example.json",
            "n8n/examples/autopilot-bandsintown-growth.example.json",
            "n8n/examples/autopilot-spotify-growth.example.json",
        ]
        banned = re.compile(
            r"(?i)(buy\s*streams|stream\s*bot|click\s*farm|fake\s*followers|guaranteed\s*playlist|paid\s*placement)"
        )
        for path in paths:
            self.assertIsNone(banned.search(self.read(path)), path)


if __name__ == "__main__":
    unittest.main()
