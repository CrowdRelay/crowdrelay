from pathlib import Path
import unittest

from rust_source_tree import read_rust_module

ROOT = Path(__file__).resolve().parents[1]


class AudienceIntelligenceContracts(unittest.TestCase):
    def test_migration_is_additive_and_feature_gated(self):
        source = (ROOT / "migrations/0031_audience_intelligence.sql").read_text()
        for table in ("audience_segments", "fan_audience_tags", "communication_campaigns"):
            self.assertIn(f"CREATE TABLE {table}", source)
        self.assertIn("communication_campaigns_enabled", source)
        self.assertIn("'staged rollout'", source)
        self.assertIn("FOREIGN KEY (workspace_id, dispatch_event_id)", source)
        self.assertNotIn("ALTER TABLE fan_consents", source)
        self.assertNotIn("ALTER TABLE outbox_events", source)

    def test_delivery_is_late_bound_and_consent_filtered(self):
        source = read_rust_module(ROOT, "crates/crowdrelay-api/src/audience.rs")
        self.assertIn("communication.campaign_due", source)
        self.assertIn("available_at", source)
        self.assertIn("consent.purpose = 'marketing'", source)
        self.assertIn('feature_enabled(&state, "mailer_enabled")', source)
        self.assertIn('feature_enabled(&state, "communication_campaigns_enabled")', source)
        self.assertIn("delivery_plan", source)
        self.assertIn("next_after_fan_id", source)
        self.assertIn("after_fan_id", source)
        for forbidden in ("reqwest::", "lettre::", "smtp", "send_mail("):
            self.assertNotIn(forbidden, source.lower())

    def test_paginated_dispatch_freezes_membership_but_rechecks_consent(self):
        migration = (ROOT / "migrations/0031_audience_intelligence.sql").read_text()
        source = read_rust_module(ROOT, "crates/crowdrelay-api/src/audience.rs")
        self.assertIn("CREATE TABLE communication_campaign_recipients", migration)
        self.assertIn("recipient_snapshot_at", migration)
        self.assertIn("ensure_recipient_snapshot", source)
        self.assertIn("communication_campaign_recipients snapshot", source)
        self.assertIn("fan.status = 'active'", source)
        self.assertIn("consent.purpose = 'marketing'", source)
        self.assertIn("ORDER BY fan.id", source)
        self.assertNotIn("normalized_email text", migration[migration.index("CREATE TABLE communication_campaign_recipients"):])

    def test_outbox_payload_does_not_embed_recipient_pii(self):
        source = read_rust_module(ROOT, "crates/crowdrelay-api/src/audience.rs")
        start = source.index("'communication.campaign_due'")
        end = source.index("RETURNING id", start)
        payload = source[start:end]
        self.assertNotIn("normalized_email", payload)
        self.assertNotIn("display_name", payload)
        self.assertNotIn("recipients", payload)
        self.assertNotIn("subject", payload)
        self.assertNotIn("content", payload)

    def test_currency_is_never_implicitly_mixed_in_revenue_analytics(self):
        source = read_rust_module(ROOT, "crates/crowdrelay-api/src/audience.rs")
        revenue = source[source.index("pub async fn revenue") : source.index("async fn load_fan_cards")]
        self.assertIn("GROUP BY orders.currency", revenue)
        self.assertIn("orders.currency::text AS currency", revenue)

    def test_router_and_contract_are_wired(self):
        router = ((ROOT / "crates/crowdrelay-api/src/lib.rs").read_text() + (ROOT / "crates/crowdrelay-api/src/routing.rs").read_text())
        openapi = (ROOT / "openapi/openapi.yaml").read_text()
        required = (
            "/v1/admin/audience/overview",
            "/v1/admin/audience/fans",
            "/v1/admin/audience/segments",
            "/v1/admin/communications/campaigns",
            "/v1/internal/communications/campaigns/{campaign_id}/delivery-plan",
            "/v1/internal/communications/campaigns/{campaign_id}/complete",
            "/v1/admin/analytics/funnel",
            "/v1/admin/analytics/revenue",
        )
        for route in required:
            self.assertIn(route, router)
            self.assertIn(route.removeprefix("/v1"), openapi)


    def test_admin_mutations_are_audited_atomically(self):
        source = read_rust_module(ROOT, "crates/crowdrelay-api/src/audience.rs")
        self.assertIn("async fn append_audit", source)
        for action in (
            "audience.tag.added",
            "audience.tag.removed",
            "audience.segment.created",
            "communication.campaign.created",
            "communication.campaign.scheduled",
            "communication.campaign.cancelled",
            "communication.campaign.completed",
        ):
            self.assertIn(action, source)
        self.assertIn("&mut transaction", source)
        self.assertIn("INSERT INTO audit_events", source)

    def test_fan_360_includes_synesthesia_detail(self):
        source = read_rust_module(ROOT, "crates/crowdrelay-api/src/audience.rs")
        self.assertIn("pub struct SynesthesiaTouch", source)
        self.assertIn("synesthesia: Vec<SynesthesiaTouch>", source)
        self.assertIn("JOIN synesthesia_runs run", source)
        self.assertIn("run.client_total_elapsed_ms", source)

    def test_flag_is_registered_with_fail_closed_default(self):
        source = read_rust_module(ROOT, "crates/crowdrelay-api/src/ecosystem.rs")
        self.assertIn('(\"communication_campaigns_enabled\", false)', source)


if __name__ == "__main__":
    unittest.main()
