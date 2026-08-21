import json
import pathlib
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]


class AutopilotDistributionExecutionContract(unittest.TestCase):
    def test_release_start_press_seeds_verified_playlist_targets(self) -> None:
        sql = (ROOT / "migrations/0072_release_playlist_outreach.sql").read_text()
        for needle in (
            "milestone <> 'start_press'",
            "target.target_kind = 'playlist'",
            "target.verified",
            "target.accepts_outreach",
            "NOT target.do_not_contact",
            "release.playlist.v1",
            "ON CONFLICT (workspace_id, source, target_id, subject_kind, subject_key)",
        ):
            self.assertIn(needle, sql)

    def test_outreach_executor_claims_before_provider_and_reports_receipt(self) -> None:
        path = ROOT / "n8n/examples/autopilot-outreach-executor.example.json"
        workflow = json.loads(path.read_text())
        nodes = workflow["nodes"]
        names = {node["name"]: node for node in nodes}
        self.assertIn("Claim action once", names)
        self.assertIn("Send Gmail pitch", names)
        self.assertIn("Submit verified free form", names)
        self.assertIn("Report provider receipt", names)
        self.assertEqual(names["Send Gmail pitch"]["type"], "n8n-nodes-base.gmail")

        encoded = path.read_text()
        self.assertIn("/execution-claim", encoded)
        self.assertIn("/execution-report", encoded)
        self.assertIn("claim_token", encoded)
        self.assertIn("provider_reference", encoded)
        self.assertIn("VIRYA_OUTREACH_FORM_ROUTES_JSON", encoded)
        self.assertIn("route.verified!==true", encoded)
        self.assertIn("route.free!==true", encoded)
        self.assertIn("route.requires_login===true", encoded)
        self.assertIn("route.requires_captcha===true", encoded)
        self.assertIn("one follow-up is the maximum", encoded)

    def test_inbound_gmail_reply_closes_outreach_loop(self) -> None:
        path = ROOT / "n8n/examples/autopilot-outreach-reply-monitor.example.json"
        workflow = json.loads(path.read_text())
        names = {node["name"]: node for node in workflow["nodes"]}
        self.assertEqual(names["Watch Gmail inbox"]["type"], "n8n-nodes-base.gmailTrigger")
        encoded = path.read_text()
        self.assertIn("/v1/internal/autopilot/provider-actions/", encoded)
        self.assertIn("/v1/admin/autopilot/outreach-targets/", encoded)
        self.assertIn("/v1/admin/autopilot/booking-targets/", encoded)
        self.assertIn("disposition:'received'", encoded)
        self.assertIn("gmail-inbound:", encoded)

    def test_examples_do_not_persist_execution_payloads(self) -> None:
        for filename in (
            "autopilot-outreach-executor.example.json",
            "autopilot-outreach-reply-monitor.example.json",
        ):
            workflow = json.loads((ROOT / "n8n/examples" / filename).read_text())
            settings = workflow["settings"]
            self.assertEqual(settings["saveDataErrorExecution"], "none")
            self.assertEqual(settings["saveDataSuccessExecution"], "none")
            self.assertFalse(settings["saveManualExecutions"])


if __name__ == "__main__":
    unittest.main()
