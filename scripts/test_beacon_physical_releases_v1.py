#!/usr/bin/env python3
"""Fail-closed source contract for Latarnik physical-release fulfillment v1.

This does not pretend to replace compiler/DB integration tests. It guards the
cross-module invariants that are easiest to accidentally weaken while those
runtime gates live in CI.
"""
from __future__ import annotations

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MIGRATION = (ROOT / "migrations/0058_beacon_physical_release_campaigns.sql").read_text()
RELEASES = (ROOT / "crates/crowdrelay-api/src/beacon_signal/releases.rs").read_text()
ADMIN = (ROOT / "crates/crowdrelay-api/src/beacon_signal/releases/admin.rs").read_text()
MEMBER = (ROOT / "crates/crowdrelay-api/src/beacon_signal/releases/member.rs").read_text()
INVITE_COPY = (ROOT / "crates/crowdrelay-api/src/beacon_signal/invite_copy.rs").read_text()
ROUTER = (ROOT / "crates/crowdrelay-api/src/routing.rs").read_text()
OPENAPI = (ROOT / "openapi/openapi.yaml").read_text()
META = (ROOT / "crates/crowdrelay-api/src/meta.rs").read_text()
RETENTION = (ROOT / "crates/crowdrelay-worker/src/retention.rs").read_text()
RETENTION_STEPS = (ROOT / "crates/crowdrelay-worker/src/retention/steps.rs").read_text()
# SQL writes were extracted from the API layer behind repository ports (S3.2-S3.6).
# The infra adapter now owns every INSERT/UPDATE/DELETE the tests guard.
INFRA_ADMIN = (ROOT / "crates/crowdrelay-infra/src/beacon_signal/admin.rs").read_text()
INFRA_MOD = (ROOT / "crates/crowdrelay-infra/src/beacon_signal/mod.rs").read_text()


class BeaconPhysicalReleasesV1Contract(unittest.TestCase):
    def test_schema_extends_existing_beacon_and_commerce_sources_of_truth(self) -> None:
        self.assertIn("ALTER TABLE viryaos_beacon_signal_profiles", MIGRATION)
        self.assertIn("'releases'", MIGRATION)
        self.assertIn("CREATE TABLE viryaos_beacon_release_campaigns", MIGRATION)
        self.assertIn("CREATE TABLE viryaos_beacon_release_recipients", MIGRATION)
        self.assertIn("REFERENCES viryaos_beacons (workspace_id, id)", MIGRATION)
        self.assertIn("REFERENCES merch_variants (workspace_id, id)", MIGRATION)
        self.assertNotIn("CREATE TABLE viryaos_latarnicy", MIGRATION)
        self.assertNotIn("CREATE TABLE beacon_inventory", MIGRATION)

    def test_launch_is_full_pool_fail_closed_and_reserves_real_stock(self) -> None:
        launch = INFRA_ADMIN.split("async fn launch_release_campaign", 1)[1].split(
            "async fn close_release_campaign", 1
        )[0]
        for guard in (
            "profile.status='active'",
            "'releases'=ANY(profile.topics)",
            "beacon.active AND beacon.verified AND beacon.accepts_outreach",
            "NOT beacon.do_not_contact",
            "active_release_count != eligible.len() as i64 || eligible.is_empty()",
            "available < i64::from(eligible_count)",
            "'campaign'",
            "inventory_reservation_items",
            "eligible_count",
        ):
            self.assertIn(guard, launch)
        self.assertNotIn("join_all", launch)

    def test_release_mail_is_server_owned_and_shipping_pii_never_enters_outbox(self) -> None:
        self.assertIn("Dziękujemy Latarniku", INFRA_MOD)
        self.assertIn("Paczkomat", INFRA_MOD)
        self.assertIn("784947481", INFRA_MOD)
        launch = INFRA_ADMIN.split("crowdrelay.beacon.release_delivery_confirmation_requested", 1)[1].split(
            ".execute(&mut *tx)", 1
        )[0]
        for field in ("subject", "text", "contact_email", "member_url"):
            self.assertRegex(launch, rf"[\"\']{re.escape(field)}[\"\']")
        self.assertIn("FROM unnest(", launch)
        self.assertIn("AS mail(beacon_id,display_name,contact_email,subject,body_text,request_id)", launch)
        for shipping_pii in ("recipient_name", "recipient_phone", "parcel_locker_code"):
            self.assertNotIn(shipping_pii, launch)
        confirmed = MEMBER.split("crowdrelay.beacon.release_delivery_confirmed", 1)[1].split(
            ".execute(&mut *tx)", 1
        )[0]
        for shipping_pii in ("recipient_name", "recipient_phone", "parcel_locker_code"):
            self.assertNotIn(shipping_pii, confirmed)

    def test_confirmation_does_not_issue_stock_and_sent_does_exactly_one_promotional_issue(self) -> None:
        confirm = MEMBER.split("pub async fn confirm_release_delivery", 1)[1].split(
            "pub async fn decline_release_delivery", 1
        )[0]
        self.assertIn("status='confirmed'", confirm)
        self.assertNotIn("inventory_ledger", confirm)
        self.assertNotIn("promotional_issue", confirm)

        update = INFRA_ADMIN.split("async fn update_release_recipient", 1)[1]
        sent = update.split('if command.status == "sent"', 1)[1].split(
            '} else if command.status == "cancelled"', 1
        )[0]
        self.assertIn("inventory_ledger", sent)
        self.assertIn("-1,'promotional_issue'", sent)
        self.assertIn("inventory_reservation_items", sent)
        self.assertIn("ON CONFLICT (workspace_id,idempotency_key) DO NOTHING", sent)

    def test_decline_and_cancel_release_reservation_without_inventory_issue(self) -> None:
        decline = MEMBER.split("pub async fn decline_release_delivery", 1)[1]
        self.assertIn("inventory_reservation_items", decline)
        self.assertIn("status='declined'", decline)
        self.assertNotIn("inventory_ledger", decline)
        self.assertNotIn("promotional_issue", decline)

        update = INFRA_ADMIN.split("async fn update_release_recipient", 1)[1]
        cancelled = update.split('} else if command.status == "cancelled"', 1)[1].split(
            "let (timestamp_column, purge)", 1
        )[0]
        self.assertIn("inventory_reservation_items", cancelled)
        self.assertNotIn("inventory_ledger", cancelled)

    def test_member_delivery_is_authenticated_topic_scoped_and_deadline_bounded(self) -> None:
        confirm = MEMBER.split("pub async fn confirm_release_delivery", 1)[1].split(
            "pub async fn decline_release_delivery", 1
        )[0]
        self.assertIn("authorize_beacon(&state, &headers)", confirm)
        self.assertIn('topic == "releases"', confirm)
        self.assertIn("campaign.status='open'", confirm)
        self.assertIn("row.2 <= OffsetDateTime::now_utc()", confirm)
        self.assertIn("phone_digits", confirm)
        self.assertIn("locker_valid", confirm)

    def test_operator_mutations_are_idempotent_and_append_only_audited(self) -> None:
        for action in (
            "create_beacon_release_campaign",
            "launch_beacon_release_campaign",
            "close_beacon_release_campaign",
            "update_beacon_release_recipient",
        ):
            self.assertIn(action, INFRA_ADMIN)
        self.assertIn("idempotency_key(&headers)", ADMIN)
        self.assertIn("record_operator_action", INFRA_ADMIN)
        self.assertNotRegex(INFRA_ADMIN, r"DELETE\s+FROM\s+operator_actions")

    def test_shipping_pii_has_a_real_bounded_retention_executor(self) -> None:
        confirm = MEMBER.split("pub async fn confirm_release_delivery", 1)[1].split(
            "pub async fn decline_release_delivery", 1
        )[0]
        self.assertIn("delivery_details_purge_after=NULL", confirm)
        self.assertNotIn("interval '180 days'", confirm)
        self.assertIn("BeaconReleaseDeliveryPii", RETENTION)
        self.assertIn("beacon_release_delivery_pii_purged", RETENTION)
        self.assertIn("scrub_beacon_release_delivery_pii", RETENTION_STEPS)
        self.assertIn("recipient.status='delivered'", RETENTION_STEPS)
        self.assertIn("recipient.delivery_details_purge_after <= now()", RETENTION_STEPS)
        self.assertIn("recipient_name=NULL,recipient_phone=NULL,parcel_locker_code=NULL", RETENTION_STEPS)
        self.assertIn("pii_purged_at=now()", RETENTION_STEPS)
        self.assertIn("OR pii_purged_at IS NOT NULL", MIGRATION)

    def test_invitation_explains_selected_release_access_without_quid_pro_quo(self) -> None:
        for phrase in (
            "wybranych materiałów i pul promocyjnych",
            "akredytację",
            "publikacja za wejściówkę",
            "Nie ma obowiązku publikowania",
            "odpisz na tę wiadomość",
        ):
            self.assertIn(phrase, INVITE_COPY)
        self.assertNotIn("każdą nową fizyczną płytę Viryi", INVITE_COPY)

    def test_route_meta_and_openapi_contract_are_complete(self) -> None:
        routes = (
            "/v1/beacon/me/releases",
            "/v1/beacon/me/releases/{campaign_id}/delivery",
            "/v1/beacon/me/releases/{campaign_id}/decline",
            "/v1/admin/autopilot/beacon-release-campaigns",
            "/v1/admin/autopilot/beacon-release-campaigns/{campaign_id}/launch",
            "/v1/admin/autopilot/beacon-release-campaigns/{campaign_id}/close",
            "/v1/admin/autopilot/beacon-release-campaigns/{campaign_id}/recipients",
            "/v1/admin/autopilot/beacon-release-campaigns/{campaign_id}/recipients/{beacon_id}",
        )
        for route in routes:
            self.assertIn(route, ROUTER)
            self.assertIn(route.removeprefix("/v1") + ":", OPENAPI)
        self.assertIn("", META)
        self.assertIn('(\"beacon_physical_releases_v1\", true)', META)


if __name__ == "__main__":
    unittest.main()
