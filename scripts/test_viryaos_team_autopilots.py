#!/usr/bin/env python3
"""Lean source contract for team-facing VIRYA OS automations.

This intentionally checks functional safety boundaries, not query plans or
implementation trivia.
"""
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]

def text(path: str) -> str:
    return (ROOT / path).read_text()

class TeamAutopilotsContract(unittest.TestCase):
    def test_three_new_ddd_contexts_are_wired_end_to_end(self):
        model = text("crates/crowdrelay-application/src/autopilot/model.rs")
        migration = text("migrations/0039_viryaos_team_autopilots.sql")
        for value in ("Release", "LiveOpportunity", "Funding"):
            self.assertIn(value, model)
        for value in ("'release'", "'live_opportunity'", "'funding'"):
            self.assertIn(value, migration)

    def test_release_drives_calendar_campaign_press_patronage_and_endorsement(self):
        source = text("crates/crowdrelay-infra/src/autopilot/operations/execution.rs")
        self.assertIn("viryaos.calendar.upsert_requested", source)
        self.assertIn("communication.campaign_due", source)
        self.assertIn("release.press.v1", source)
        self.assertIn("release.media_patronage.v1", source)
        self.assertIn("release.endorsement.v1", source)
        self.assertIn('FanWarmup => "referral"', source)

    def test_fan_growth_has_welcome_followup_and_reactivation(self):
        domain = text("crates/crowdrelay-domain/src/audience_lifecycle.rs")
        evaluator = text("crates/crowdrelay-application/src/autopilot/evaluate.rs")
        for value in ("Welcome", "SynesthesiaFollowUp", "DormantReactivation"):
            self.assertIn(value, domain)
        for template in ("viryaos.fan.welcome.v1", "viryaos.synesthesia.follow_up.v1", "viryaos.fan.reactivation.v1"):
            self.assertIn(template, evaluator)

    def test_live_auto_application_is_fee_contract_and_exclusivity_bounded(self):
        domain = text("crates/crowdrelay-domain/src/live_opportunities.rs")
        self.assertIn("snapshot.application_fee_minor <= policy.max_auto_application_fee_minor", domain)
        self.assertIn("!snapshot.requires_contract", domain)
        self.assertIn("!snapshot.exclusive", domain)
        self.assertIn("SubmitAutomatically", domain)
        self.assertIn("PrepareForApproval", domain)

    def test_funding_prepares_automatically_but_submission_is_approval(self):
        domain = text("crates/crowdrelay-domain/src/funding.rs")
        evaluator = text("crates/crowdrelay-application/src/autopilot/evaluate.rs")
        self.assertIn("PreparePackage", domain)
        self.assertIn("SubmitForApproval", domain)
        self.assertIn("force_approval", evaluator)

    def test_media_patronage_reuses_relationship_aware_outreach(self):
        domain = text("crates/crowdrelay-domain/src/outreach.rs")
        migration = text("migrations/0039_viryaos_team_autopilots.sql")
        self.assertIn("MediaPatronage", domain)
        self.assertIn("media_patronage", migration)
        self.assertIn("target_last_outreach_at", domain)

    def test_approval_actions_emit_one_provider_neutral_notification(self):
        persistence = text("crates/crowdrelay-infra/src/autopilot/decisions.rs")
        self.assertIn("viryaos.autopilot.approval_requested", persistence)
        self.assertIn('status == "awaiting_approval"', persistence)

    def test_signal_understands_new_and_existing_ticket_capacity_actions(self):
        models = text("../virya-signal/src/models.rs")
        for variant in ("ChangeTicketCapacity", "ExecuteReleaseMilestone", "ApplyLiveOpportunity", "PrepareFundingPackage", "SubmitFundingApplication"):
            self.assertIn(variant, models)

    def test_no_meta_ads_executor_was_added(self):
        combined = "\n".join(p.read_text(errors="ignore") for p in ROOT.rglob("*.rs"))
        self.assertNotIn("MetaAdsExecutor", combined)
        self.assertNotIn("meta_ads_executor", combined)

    def test_provider_submission_needs_positive_callback(self):
        migration = text("migrations/0039_viryaos_team_autopilots.sql")
        execution = text("crates/crowdrelay-infra/src/autopilot/operations/execution.rs")
        ingress = text("crates/crowdrelay-infra/src/autopilot/operations/ingress.rs")
        self.assertIn("submission_requested", migration)
        self.assertIn("SET status='submission_requested'", execution)
        self.assertIn("TeamOpportunityProgress::Submitted", ingress)
        self.assertIn("status='submission_requested'", ingress)

    def test_opportunity_money_is_currency_explicit(self):
        migration = text("migrations/0039_viryaos_team_autopilots.sql")
        api = text("openapi/openapi.yaml")
        executor = text("crates/crowdrelay-infra/src/autopilot/operations/execution.rs")
        self.assertIn("currency text NOT NULL", migration)
        self.assertIn("currency: { type: string", api)
        self.assertIn('"currency": row.', executor)

    def test_festival_scout_facts_are_scored_in_rust_domain(self):
        domain = text("crates/crowdrelay-domain/src/live_opportunities.rs")
        api = text("crates/crowdrelay-api/src/autopilot.rs")
        discovery = text("crates/crowdrelay-api/src/autopilot/discovery.rs")
        routing = text("crates/crowdrelay-api/src/routing.rs")
        self.assertIn("evaluate_live_opportunity_discovery", domain)
        self.assertIn("discover_team_opportunity", api)
        self.assertIn("/v1/admin/autopilot/team-opportunities/discover", routing)
        self.assertIn('"destination_unverified": true', discovery)

    def test_live_auto_submit_requires_a_real_executor_destination(self):
        domain = text("crates/crowdrelay-domain/src/live_opportunities.rs")
        decisions = text("crates/crowdrelay-infra/src/autopilot/decisions.rs")
        self.assertIn("auto_submission_capable", domain)
        self.assertIn("snapshot.auto_submission_capable", domain)
        self.assertIn("submission_adapter", decisions)
        self.assertIn("contact_email", decisions)

    def test_fan_message_intent_contains_verified_delivery_identity(self):
        actions = text("crates/crowdrelay-infra/src/autopilot/actions.rs")
        self.assertIn("viryaos.fan_lifecycle.message_requested", actions)
        self.assertIn('"email": fan.0', actions)
        self.assertIn('"display_name": fan.1', actions)
        self.assertIn('"locale": fan.2', actions)

    def test_query_plan_regression_script_is_gone(self):
        self.assertFalse((ROOT / "scripts/query-plan-regression.py").exists())
        workflows = text(".github/workflows/ci.yml") + text(".github/workflows/performance.yml")
        self.assertNotIn("query-plan-regression.py", workflows)

if __name__ == "__main__":
    unittest.main()
