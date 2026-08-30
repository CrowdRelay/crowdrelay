#!/usr/bin/env python3
"""Full Latarnik lifecycle release contract.

Guards the v2 lifecycle additions that are easy to regress without a live DB:
CRM ownership, consent fail-closed auth, bounded invite/notification fanout,
monotonic Beacon×Event engagement, press-room management and coverage.
"""
from __future__ import annotations

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MIGRATION = (ROOT / "migrations/0057_beacon_signal_full_mode.sql").read_text()
API = (ROOT / "crates/crowdrelay-api/src/beacon_signal.rs").read_text()
INVITE_COPY = (ROOT / "crates/crowdrelay-api/src/beacon_signal/invite_copy.rs").read_text()
LIFECYCLE = (ROOT / "crates/crowdrelay-api/src/beacon_signal/lifecycle.rs").read_text()
ADMIN = (ROOT / "crates/crowdrelay-api/src/beacon_signal/lifecycle/admin.rs").read_text()
MEMBER = (ROOT / "crates/crowdrelay-api/src/beacon_signal/lifecycle/member.rs").read_text()
ROUTING = (ROOT / "crates/crowdrelay-api/src/routing.rs").read_text()
META = (ROOT / "crates/crowdrelay-api/src/meta.rs").read_text()
OPENAPI = (ROOT / "openapi/openapi.yaml").read_text()
WORKER = (ROOT / "crates/crowdrelay-worker/src/push_delivery/repository.rs").read_text()
# SQL writes were extracted from the API layer behind repository ports (S3.2-S3.6).
INFRA_SIGNAL = (ROOT / "crates/crowdrelay-infra/src/beacon_signal/signal.rs").read_text()


class BeaconSignalModeV2Contract(unittest.TestCase):
    def test_schema_models_full_lifecycle_without_second_crm(self) -> None:
        for table in (
            "viryaos_beacon_signal_event_engagements",
            "viryaos_beacon_signal_coverage",
            "viryaos_beacon_press_assets",
        ):
            self.assertIn(f"CREATE TABLE {table}", MIGRATION)
        self.assertIn("REFERENCES viryaos_beacons (workspace_id, id) ON DELETE CASCADE", MIGRATION)
        self.assertIn("REFERENCES events (workspace_id, id) ON DELETE SET NULL (event_id)", MIGRATION)
        self.assertIn("'eligible','notified','opened','interested','helping','completed','declined'", MIGRATION)
        self.assertIn("UNIQUE (workspace_id, beacon_id, event_id, url)", MIGRATION)
        self.assertIn("UNIQUE NULLS NOT DISTINCT (workspace_id, asset_key, event_id)", MIGRATION)

    def test_auth_and_delivery_both_recheck_current_consent(self) -> None:
        auth = re.search(r"async fn authorize_beacon[\s\S]+?fetch_optional", API)
        self.assertIsNotNone(auth)
        block = auth.group(0) if auth else ""
        for guard in (
            "profile.status = 'active'",
            "beacon.active",
            "beacon.verified",
            "beacon.accepts_outreach",
            "NOT beacon.do_not_contact",
            "session.revoked_at IS NULL",
            "session.expires_at > now()",
        ):
            self.assertIn(guard, block)
        for guard in ("beacon.verified", "accepts_outreach", "do_not_contact"):
            self.assertIn(guard, WORKER)

    def test_batch_invites_are_bounded_hashed_and_revoke_old_sessions(self) -> None:
        self.assertIn("const MAX_BATCH_INVITES: usize = 200", LIFECYCLE)
        self.assertIn("payload.beacon_ids.len() > MAX_BATCH_INVITES", ADMIN)
        self.assertIn("token_hash(&invite_token)", LIFECYCLE)
        self.assertNotIn("invite_token text", MIGRATION)
        self.assertNotIn("INSERT INTO outbox_events", ADMIN.split("pub async fn create_invite_batch", 1)[1].split("pub async fn admin_dashboard", 1)[0])
        self.assertIn("SET revoked_at=COALESCE(revoked_at, now())", LIFECYCLE)
        self.assertIn("COALESCE(profile.status, '') <> 'active'", LIFECYCLE)
        self.assertIn("InviteDeliveryCopy", API)
        self.assertIn("version: 2", API)
        self.assertIn("version: u8", LIFECYCLE)
        self.assertIn("version: 2", LIFECYCLE)
        self.assertIn("invite_delivery_copy", LIFECYCLE)
        self.assertIn("Virya Signal — zaproszenie do Latarnika", INVITE_COPY)
        self.assertIn("Virya Signal — Beacon invitation", INVITE_COPY)

    def test_event_lifecycle_and_campaign_projection_are_monotonic(self) -> None:
        self.assertIn("let next_status = match current.as_deref()", INFRA_SIGNAL)
        self.assertIn('Some("completed")', INFRA_SIGNAL)
        self.assertIn('Some("declined")', INFRA_SIGNAL)
        self.assertIn("let (campaign_status, campaign_disposition) = match next_status", INFRA_SIGNAL)
        self.assertIn("viryaos_beacon_campaigns.status='partner' THEN 'partner'", INFRA_SIGNAL)
        self.assertIn("status NOT IN ('suppressed','closed')", INFRA_SIGNAL)
        self.assertIn("viryaos_beacon_campaigns.status='declined' THEN 'declined'", INFRA_SIGNAL)
        self.assertNotIn("let campaign_status = match payload.action", INFRA_SIGNAL)
        self.assertIn("crowdrelay.beacon.signal_engagement_recorded", INFRA_SIGNAL)

    def test_press_room_requests_assets_and_coverage_are_complete(self) -> None:
        for symbol in (
            "press_room",
            "my_press_requests",
            "submit_coverage",
            "admin_resolve_press_request",
            "admin_upsert_press_asset",
            "admin_coverage",
        ):
            self.assertIn(symbol, API)
        self.assertIn("valid_https_url", MEMBER)
        self.assertIn("valid_press_url", ADMIN)
        self.assertIn("crowdrelay.beacon.coverage_submitted", INFRA_SIGNAL)
        self.assertIn("crowdrelay.beacon.press_request_resolved", ADMIN)
        self.assertIn("viryaos_beacon_press_assets", MEMBER)
        self.assertIn("PressRoomEventView", MEMBER)
        self.assertIn("event.description", MEMBER)
        self.assertIn("event.trailer_url", MEMBER)

    def test_nearby_waves_are_bounded_idempotent_and_consent_aware(self) -> None:
        for guard in (
            "profile.status='active'",
            "profile.nearby_gigs_enabled",
            "'shows'=ANY(profile.topics)",
            "beacon.active AND beacon.verified AND beacon.accepts_outreach",
            "NOT beacon.do_not_contact",
            "LIMIT $2",
            "last_notified_at IS NULL",
            "ON CONFLICT (workspace_id,source_kind,source_id,endpoint_id) DO NOTHING",
            "?event_id=' || ranked.event_id::text",
        ):
            self.assertIn(guard, INFRA_SIGNAL)
        self.assertIn("DEFAULT_WAVE_SIZE: i64 = 20", API)
        self.assertIn("MAX_WAVE_SIZE: i64 = 100", API)
        self.assertNotIn("join_all", API)

    def test_operator_and_member_route_surface_is_wired_and_documented(self) -> None:
        routes = (
            "/v1/beacon/me/press-room",
            "/v1/beacon/me/press-requests",
            "/v1/beacon/me/events/{event_id}/engagement",
            "/v1/beacon/me/events/{event_id}/coverage",
            "/v1/beacon/me/leave",
            "/v1/admin/autopilot/beacons/signal-invites/batch",
            "/v1/admin/autopilot/beacon-signal",
            "/v1/admin/autopilot/beacon-signal/candidates",
            "/v1/admin/autopilot/beacons/{beacon_id}/signal-state",
            "/v1/admin/autopilot/beacon-press-assets",
            "/v1/admin/autopilot/beacon-signal-engagements",
            "/v1/admin/autopilot/beacon-coverage",
            "/v1/admin/autopilot/beacon-press-requests/{press_request_id}/resolve",
        )
        for route in routes:
            self.assertIn(route, ROUTING)
            self.assertIn(route.replace("/v1", "", 1) + ":", OPENAPI)
        self.assertIn('("beacon_signal_v2", true)', META)
        self.assertIn("", META)

    def test_leave_is_channel_scoped_unless_global_dnc_is_explicit(self) -> None:
        self.assertIn("do_not_contact: bool", LIFECYCLE)
        self.assertIn("UPDATE viryaos_beacon_signal_sessions", INFRA_SIGNAL)
        self.assertIn("audience_kind='beacon'", INFRA_SIGNAL)
        self.assertIn("if command.do_not_contact", INFRA_SIGNAL)
        self.assertIn("accepts_outreach=false,do_not_contact=true", INFRA_SIGNAL)
        self.assertIn("crowdrelay.beacon.signal_left", INFRA_SIGNAL)


if __name__ == "__main__":
    unittest.main()
