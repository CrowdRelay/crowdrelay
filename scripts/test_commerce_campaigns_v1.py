from pathlib import Path
import hashlib
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
MIGRATION = (ROOT / "migrations/0027_merch_inventory_reward_campaigns.sql").read_text(encoding="utf-8")
COMMERCE = (ROOT / "crates/crowdrelay-api/src/commerce.rs").read_text(encoding="utf-8")
ROUTER = (ROOT / "crates/crowdrelay-api/src/lib.rs").read_text(encoding="utf-8")
FLAGS = (ROOT / "crates/crowdrelay-api/src/ecosystem.rs").read_text(encoding="utf-8")
WORKER = (ROOT / "crates/crowdrelay-worker/src/draws.rs").read_text(encoding="utf-8")
OPENAPI = (ROOT / "openapi/openapi.yaml").read_text(encoding="utf-8")

PROTECTED_TABLES = {
    "fans",
    "fan_access_tokens",
    "ticket_orders",
    "ticket_order_items",
    "outbox_events",
    "webhook_deliveries",
}
PHYSICAL_REWARD_OUTBOX_SHA256 = "30c9bf0dedd7b61b10e55cfddde201618d70d5cc03232675833d5083f86efd9d"


def strip_sql_comments(value: str) -> str:
    return "\n".join(line.split("--", 1)[0] for line in value.splitlines())


def physical_reward_outbox_block() -> str:
    anchor = (
        '        append_outbox(\n'
        '            transaction,\n'
        '            draw.workspace_id,\n'
        '            "physical_reward.granted",'
    )
    start = WORKER.index(anchor)
    end = WORKER.index("        .await?;", start) + len("        .await?;")
    return WORKER[start:end]


class CommerceCampaignsV1(unittest.TestCase):
    def test_migration_is_additive_and_flags_start_disabled(self):
        sql = strip_sql_comments(MIGRATION).lower()
        self.assertNotRegex(sql, r"\b(drop|truncate)\s+(table\s+)?")
        for table in PROTECTED_TABLES:
            self.assertNotRegex(sql, rf"\balter\s+table\s+{re.escape(table)}\b")
            self.assertNotRegex(sql, rf"\b(delete\s+from|update)\s+{re.escape(table)}\b")
        for key in (
            "merch_inventory_enabled",
            "reward_campaigns_enabled",
            "merch_inventory_writes_enabled",
        ):
            self.assertIn(f"('{key}')", MIGRATION)
            self.assertIn(f'("{key}", false)', FLAGS)

    def test_privileged_routes_reuse_existing_namespace_authentication(self):
        for path in (
            "/v1/internal/merch/reservations",
            "/v1/admin/merch/catalog",
            "/v1/admin/reward-campaigns",
            "/v1/admin/merch/promotion-recommendations",
            "/v1/staff/reward-fulfillments",
        ):
            self.assertIn(path, ROUTER)
        self.assertIn('path.starts_with("/v1/admin/")', ROUTER)
        self.assertIn('path.starts_with("/v1/staff/")', ROUTER)
        self.assertIn('path.starts_with("/v1/commerce/") || path.starts_with("/v1/internal/")', ROUTER)

    def test_inventory_writes_are_kill_switched_but_reconciliation_is_not(self):
        for function in (
            "upsert_catalog_inner",
            "adjust_inventory_inner",
            "reserve_inventory_inner",
            "create_reward_campaign_inner",
        ):
            start = COMMERCE.index(f"async fn {function}")
            next_function = COMMERCE.find("\nasync fn ", start + 1)
            block = COMMERCE[start: next_function if next_function != -1 else None]
            self.assertIn("require_inventory_writes(state).await?", block)
        for function in ("commit_inventory_inner", "release_inventory_inner"):
            start = COMMERCE.index(f"async fn {function}")
            next_function = COMMERCE.find("\nasync fn ", start + 1)
            block = COMMERCE[start: next_function if next_function != -1 else None]
            self.assertNotIn("require_inventory_writes", block)

    def test_cancelled_or_released_campaigns_report_zero_live_reserved_stock(self):
        self.assertIn("LEFT JOIN inventory_reservations AS allocation_reservation", COMMERCE)
        self.assertIn("allocation_reservation.status = 'active'", COMMERCE)
        self.assertIn("reservation_item.reservation_id = allocation_reservation.id", COMMERCE)


    def test_paid_stripe_event_commits_even_after_out_of_order_release(self):
        start = COMMERCE.index("async fn commit_inventory_inner")
        end = COMMERCE.index("\nasync fn release_inventory_inner", start)
        block = COMMERCE[start:end]
        self.assertIn('"active" | "expired" | "released" => {}', block)
        self.assertIn("status IN ('active', 'expired', 'released')", block)
        self.assertIn("ON CONFLICT (workspace_id, idempotency_key) DO NOTHING", block)
        self.assertIn("released_at = NULL, release_reason = NULL", block)


    def test_giveaway_analysis_is_conservative_when_history_is_short_or_sales_rise(self):
        self.assertIn("history_days < 30", COMMERCE)
        self.assertIn("available_quantity / 4", COMMERCE)
        self.assertIn("sold_30d >= 3", COMMERCE)
        self.assertIn("THEN 0", COMMERCE)
        self.assertIn("upcoming_events_60d * 2", COMMERCE)

    def test_existing_physical_reward_mail_event_contract_is_byte_stable(self):
        digest = hashlib.sha256(physical_reward_outbox_block().encode()).hexdigest()
        self.assertEqual(digest, PHYSICAL_REWARD_OUTBOX_SHA256)

    def test_openapi_and_router_publish_the_same_core_contracts(self):
        for path in (
            "/v1/public/merch/catalog",
            "/v1/internal/merch/reservations",
            "/v1/admin/reward-campaigns",
            "/v1/admin/merch/promotion-recommendations",
            "/v1/admin/reward-fulfillments/{winner_id}",
        ):
            self.assertIn(path, ROUTER)
            self.assertIn(path.removeprefix("/v1") + ":", OPENAPI)

    def test_new_critical_module_has_no_panic_shortcuts(self):
        self.assertNotRegex(COMMERCE, r"\.(unwrap|expect)\s*\(")
        self.assertNotRegex(COMMERCE, r"\b(todo|unimplemented|panic)!\s*\(")


if __name__ == "__main__":
    unittest.main()
