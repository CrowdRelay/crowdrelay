#!/usr/bin/env python3
from pathlib import Path
import unittest

from rust_source_tree import read_rust_module

ROOT = Path(__file__).resolve().parents[1]


class CommunicationDeliveryLedgerContracts(unittest.TestCase):
    def test_migration_keeps_pii_out_and_claims_unique(self):
        migration = (ROOT / "migrations/0043_communication_delivery_ledger.sql").read_text()
        self.assertIn("CREATE TABLE communication_campaign_deliveries", migration)
        self.assertIn("communication_campaign_deliveries_recipient_fk", migration)
        self.assertIn("communication_campaign_deliveries_attempt_key_idx", migration)
        self.assertIn("status IN ('claimed', 'delivered', 'failed')", migration)
        for forbidden in ("email", "display_name", "normalized_email", "message_body"):
            self.assertNotIn(forbidden, migration.lower())

    def test_provider_send_is_claimed_and_ambiguous_replays_fail_closed(self):
        source = read_rust_module(ROOT, "crates/crowdrelay-api/src/audience.rs")
        self.assertIn("claim_campaign_delivery", source)
        self.assertIn("report_campaign_delivery", source)
        self.assertIn("send_allowed: true", source)
        self.assertIn("send_allowed: false", source)
        self.assertIn("claim_expired_unknown", source)
        self.assertIn("DELIVERY_CLAIM_TTL_MINUTES", source)
        self.assertIn("attempt_key", source)

    def test_delivery_plan_only_returns_unclaimed_recipients(self):
        source = read_rust_module(ROOT, "crates/crowdrelay-api/src/audience.rs")
        self.assertIn("LEFT JOIN communication_campaign_deliveries delivery", source)
        self.assertIn("AND delivery.fan_id IS NULL", source)
        self.assertIn("DeliveryProgress", source)
        self.assertIn("pending_count", source)
        self.assertIn("claimed_count", source)

    def test_completed_delivery_remains_in_authoritative_counts_after_opt_out(self):
        source = read_rust_module(ROOT, "crates/crowdrelay-api/src/audience.rs")
        start = source.index("async fn delivery_progress")
        body = source[start:]
        self.assertIn("delivery.fan_id IS NOT NULL OR", body)
        self.assertIn("delivery.fan_id IS NULL AND", body)
        self.assertIn("delivery.status = 'delivered'", body)
        self.assertIn("delivery.status = 'failed'", body)

    def test_completion_is_ledger_authoritative(self):
        source = read_rust_module(ROOT, "crates/crowdrelay-api/src/audience.rs")
        completion = source[source.index("pub async fn complete_campaign") :]
        self.assertIn("delivery_progress", completion)
        self.assertIn("progress.pending_count != 0", completion)
        self.assertIn("progress.claimed_count != 0", completion)
        self.assertIn("payload.recipient_count != recipient_count", completion)

    def test_internal_delivery_endpoints_require_commerce_auth(self):
        source = read_rust_module(ROOT, "crates/crowdrelay-api/src/audience.rs")
        function_names = (
            "delivery_plan",
            "claim_campaign_delivery",
            "report_campaign_delivery",
            "complete_campaign",
        )
        for index, name in enumerate(function_names):
            start = source.index(f"pub async fn {name}")
            end = (
                source.index(f"pub async fn {function_names[index + 1]}", start)
                if index + 1 < len(function_names)
                else len(source)
            )
            body = source[start:end]
            self.assertIn(
                "state.ticketing.commerce_authorized(&headers)",
                body,
                msg=f"{name} must fail closed without the commerce bearer",
            )
            self.assertIn("Problem::unauthorized", body)

    def test_routes_and_openapi_are_wired(self):
        router = (ROOT / "crates/crowdrelay-api/src/routing.rs").read_text()
        spec = (ROOT / "openapi/openapi.yaml").read_text()
        for route in (
            "/v1/internal/communications/campaigns/{campaign_id}/deliveries/{fan_id}/claim",
            "/v1/internal/communications/campaigns/{campaign_id}/deliveries/{fan_id}/result",
        ):
            self.assertIn(route, router)
            self.assertIn(route.removeprefix("/v1"), spec)
        self.assertIn("maximum: 500", spec)


if __name__ == "__main__":
    unittest.main()
