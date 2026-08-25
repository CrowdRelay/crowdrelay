#!/usr/bin/env python3
"""Lean source contract for team-facing VIRYA OS automations.

This intentionally checks functional safety boundaries, not query plans or
implementation trivia.
"""
from pathlib import Path
from rust_source_tree import read_rust_module
import unittest

ROOT = Path(__file__).resolve().parents[1]

def text(path: str) -> str:
    if path.endswith(".rs"):
        return read_rust_module(ROOT, path)
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
        self.assertIn("crowdrelay.calendar.upsert_requested", source)
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
        for template in ("crowdrelay.fan.welcome.v1", "crowdrelay.synesthesia.follow_up.v1", "crowdrelay.fan.reactivation.v1"):
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
        evaluator = text("crates/crowdrelay-application/src/autopilot/evaluate/commercial.rs")
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
        self.assertIn("crowdrelay.autopilot.approval_requested", persistence)
        self.assertIn('status == "awaiting_approval"', persistence)

    def test_published_action_contract_includes_team_autopilot_actions(self):
        model = text("crates/crowdrelay-application/src/autopilot/model.rs")
        api = text("openapi/openapi.yaml")
        variants = (
            ("ChangeTicketCapacity", "change_ticket_capacity"),
            ("ExecuteReleaseMilestone", "execute_release_milestone"),
            ("ApplyLiveOpportunity", "apply_live_opportunity"),
            ("PrepareFundingPackage", "prepare_funding_package"),
            ("SubmitFundingApplication", "submit_funding_application"),
        )
        for rust_variant, wire_kind in variants:
            self.assertIn(rust_variant, model)
            self.assertIn(wire_kind, api)

    def test_no_meta_ads_executor_was_added(self):
        combined = "\n".join(p.read_text(errors="ignore") for p in ROOT.rglob("*.rs"))
        self.assertNotIn("MetaAdsExecutor", combined)
        self.assertNotIn("meta_ads_executor", combined)

    def test_provider_submission_needs_positive_callback(self):
        migration = text("migrations/0039_viryaos_team_autopilots.sql")
        execution = text("crates/crowdrelay-infra/src/autopilot/operations/execution.rs")
        ingress = text("crates/crowdrelay-infra/src/autopilot/operations/ingress/team.rs")
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
        actions = text(
            "crates/crowdrelay-infra/src/autopilot/actions_execution.rs"
        )
        self.assertIn("crowdrelay.fan_lifecycle.message_requested", actions)
        self.assertIn('"email": fan.0', actions)
        self.assertIn('"display_name": fan.1', actions)
        self.assertIn('"locale": fan.2', actions)

    def test_show_growth_uses_current_free_distribution_without_spam_or_paid_leakage(self):
        execution = text("crates/crowdrelay-infra/src/autopilot/operations/show_growth_execution.rs")
        for surface in (
            "amazon_music_event_visibility_via_bandsintown_distribution",
            "songkick_partner_distribution_health_deezer_bandcamp_soundcloud",
        ):
            self.assertIn(surface, execution)
        self.assertIn("verify_youtube_sell_tickets_setting", execution)
        self.assertIn("community_posts_are_manual_or_moderator_approved", execution)
        self.assertIn("ask_verified_scene_beacon_for_one_warm_intro_to_one_relevant_local_scene_contact", execution)
        self.assertIn("warm_intro_requires_beacon_consent_and_no_private_contact_data_is_forwarded_without_permission", execution)
        self.assertIn("fan_generated_live_photo_or_clip_with_explicit_repost_permission", execution)
        self.assertIn("fan_generated_media_requires_explicit_repost_permission_and_credit", execution)
        self.assertIn("never_bypass_group_rules_posting_limits_or_moderation", execution)
        self.assertIn('"relay_pack"', execution)
        self.assertIn("send_a_personal_invite_to_one_to_three_relevant_friends", execution)
        self.assertIn("no_mass_dm_or_contact_scraping", execution)
        self.assertIn("virya_signal_signup_qr_for_merch_table_current_shows_and_permitted_partner_surfaces", execution)
        self.assertIn("owned_qr_must_preserve_normal_signal_consent_and_use_campaign_attribution", execution)

    def test_grassroots_distribution_is_durable_measurable_and_consent_bounded(self):
        domain = text("crates/crowdrelay-domain/src/show_growth.rs")
        execution = text("crates/crowdrelay-infra/src/autopilot/operations/show_growth_execution.rs")
        migration = text("migrations/0054_grassroots_distribution.sql")
        meta = text("crates/crowdrelay-api/src/meta.rs")
        for needle in (
            "GrassrootsSceneRelay",
            "grassroots_scene_relay_lead_days",
            "grassroots_scene_relay_requested",
        ):
            self.assertIn(needle, domain)
        for needle in (
            "verified_local_metal_media_or_podcast",
            "record_store",
            "rehearsal_studio",
            "tattoo_alt_fashion_or_scene_business",
            "one_consent_based_warm_intro",
            "no_scraping_no_mass_dm_no_automated_cold_group_posting",
        ):
            self.assertIn(needle, execution)
        for table in (
            "viryaos_show_growth_surfaces",
            "viryaos_grassroots_edges",
            "viryaos_grassroots_activations",
        ):
            self.assertIn(f"CREATE TABLE {table}", migration)
        self.assertIn("consent_recorded_at", migration)
        self.assertIn("attributed_ticket_orders", migration)
        self.assertIn("show_growth_surface_clicks_7d", migration)
        self.assertIn("grassroots_activation_replies_14d", migration)
        self.assertIn("", meta)
        runtime = text("crates/crowdrelay-infra/src/autopilot/runtime.rs")
        api_runtime = text("crates/crowdrelay-api/src/autopilot/runtime.rs")
        self.assertIn("record_show_growth_receipt", runtime)
        self.assertIn("viryaos_show_growth_surfaces", runtime)
        self.assertIn("viryaos_grassroots_activations", runtime)
        self.assertIn('["surfaces", "activations"]', api_runtime)

    def test_beacon_identity_dedup_keeps_distinct_email_less_scene_partners(self):
        migration = text("migrations/0053_beacon_identity_dedup.sql")
        ingress = text("crates/crowdrelay-infra/src/autopilot/operations/ingress/beacons.rs")
        actions = text(
            "crates/crowdrelay-infra/src/autopilot/actions_execution.rs"
        )
        self.assertIn("pg_get_constraintdef(con.oid)", migration)
        self.assertIn("UNIQUE NULLS NOT DISTINCT (workspace_id, beacon_kind, city_id, contact_email)", migration)
        self.assertIn("viryaos_beacons_email_identity_uq", migration)
        self.assertIn("viryaos_beacons_destination_identity_uq", migration)
        self.assertIn("WHERE contact_email IS NULL AND destination_url IS NOT NULL", migration)
        self.assertIn("$4::text IS NOT NULL AND contact_email = $4", ingress)
        self.assertIn("$4::text IS NULL AND $5::text IS NOT NULL", ingress)
        for source_class in (
            "local_metal_media_and_podcasts",
            "record_stores_rehearsal_studios_and_music_shops",
            "tattoo_alt_fashion_and_scene_businesses",
            "moderated_metal_communities_and_forums",
        ):
            self.assertIn(source_class, actions)
        self.assertIn("never_treat_generic_local_businesses_as_scene_relevant_without_public_evidence", actions)
        self.assertIn("NULLIF(btrim(destination_url), '')", migration)


    def test_query_plan_regression_script_is_gone(self):
        self.assertFalse((ROOT / "scripts/query-plan-regression.py").exists())
        workflows = text(".github/workflows/ci.yml") + text(".github/workflows/performance.yml")
        self.assertNotIn("query-plan-regression.py", workflows)
    def test_beacon_optional_urls_reject_blank_identity_values(self) -> None:
        api = (ROOT / "crates/crowdrelay-api/src/autopilot/outreach_release.rs").read_text()
        infra = (ROOT / "crates/crowdrelay-infra/src/autopilot/operations/ingress/beacons.rs").read_text()
        for source in (api, infra):
            self.assertIn("trimmed.is_empty() || trimmed.len() > 2048", source)
        self.assertIn("normalized_source", infra)


if __name__ == "__main__":
    unittest.main()
