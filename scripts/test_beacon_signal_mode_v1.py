#!/usr/bin/env python3
"""Latarnik/Beacon Signal mode release contract.

This is intentionally structural: it catches security, bounded-fanout and
cross-service drift even on runners that do not provision a live Postgres DB.
"""
from __future__ import annotations

import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MIGRATION = (ROOT / "migrations/0056_beacon_signal_mode.sql").read_text()
API = (ROOT / "crates/crowdrelay-api/src/beacon_signal.rs").read_text()
HELPERS = (ROOT / "crates/crowdrelay-api/src/beacon_signal/helpers.rs").read_text()
PUSH = (ROOT / "crates/crowdrelay-api/src/push.rs").read_text()
WORKER = (ROOT / "crates/crowdrelay-worker/src/push_delivery/repository.rs").read_text()
ROUTING = (ROOT / "crates/crowdrelay-api/src/routing.rs").read_text()
META = (ROOT / "crates/crowdrelay-api/src/meta.rs").read_text()


class BeaconSignalModeV1Contract(unittest.TestCase):
    def test_migration_keeps_beacon_as_relationship_source_of_truth(self) -> None:
        for table in (
            "viryaos_beacon_signal_profiles",
            "viryaos_beacon_signal_sessions",
            "viryaos_beacon_press_requests",
        ):
            self.assertIn(f"CREATE TABLE {table}", MIGRATION)
        self.assertIn("REFERENCES viryaos_beacons (workspace_id, id) ON DELETE CASCADE", MIGRATION)
        self.assertIn("invite_token_hash bytea", MIGRATION)
        self.assertIn("token_hash bytea NOT NULL", MIGRATION)
        self.assertNotIn("invite_token text", MIGRATION)
        self.assertIn("radius_km BETWEEN 10 AND 500", MIGRATION)
        self.assertIn("audience_kind IN ('staff','beacon')", MIGRATION)
        self.assertIn("'beacon_nearby_concert'", MIGRATION)

    def test_invites_are_eligible_single_use_and_sessions_revocable(self) -> None:
        for guard in ("beacon.active", "beacon.verified", "accepts_outreach", "do_not_contact"):
            self.assertIn(guard, API)
        self.assertIn("profile.invite_token_hash = $2", API)
        self.assertIn("FOR UPDATE OF profile", API)
        self.assertIn("invite_token_hash=NULL", API)
        self.assertIn("session.revoked_at IS NULL", API)
        self.assertIn("session.expires_at > now()", API)
        self.assertIn("Sha256::digest", HELPERS)
        self.assertIn("token_hash", API)
        self.assertNotIn("join_all", API)

    def test_nearby_wave_is_distance_ranked_bounded_and_idempotent(self) -> None:
        self.assertIn("MAX_WAVE_SIZE: i64 = 100", API)
        self.assertIn("DEFAULT_WAVE_SIZE: i64 = 20", API)
        self.assertIn("WHERE distance_km <= radius_km", API)
        self.assertIn("6371 * 2 * ASIN", API)
        self.assertRegex(API, r"ORDER BY\s+starts_at,\s*relevance_basis_points DESC")
        self.assertIn("LIMIT $2", API)
        self.assertRegex(API, r"ON CONFLICT \(workspace_id,\s*beacon_id,\s*event_id\) DO NOTHING")
        self.assertRegex(API, r"ON CONFLICT \(workspace_id,\s*source_kind,\s*source_id,\s*endpoint_id\) DO NOTHING")

    def test_press_room_and_requests_are_first_class(self) -> None:
        self.assertIn('epk_url: format!("{root}/epk")', API)
        self.assertIn("PressPhoto", API)
        self.assertIn("CleanVersion", API)
        self.assertIn("Accreditation", API)
        self.assertIn("admin_press_requests", API)

    def test_push_is_separate_from_fan_and_staff_and_worker_rechecks_auth(self) -> None:
        self.assertIn("register_beacon_endpoint", PUSH)
        self.assertIn("disable_beacon_endpoint", PUSH)
        self.assertIn("audience_kind = 'beacon'", PUSH)
        self.assertIn("beacon_session_ineligible", WORKER)
        for table in ("viryaos_beacon_signal_sessions", "viryaos_beacon_signal_profiles", "viryaos_beacons"):
            self.assertIn(table, WORKER)
        self.assertIn("session.revoked_at IS NULL", WORKER)
        self.assertIn("session.expires_at > now()", WORKER)

    def test_route_and_capability_surface_is_complete(self) -> None:
        for route in (
            "/v1/beacon/invitations/exchange",
            "/v1/beacon/me",
            "/v1/beacon/me/preferences",
            "/v1/beacon/me/press-requests",
            "/v1/beacon/me/logout",
            "/v1/beacon/push/endpoints",
            "/v1/beacon/push/endpoints/disable",
            "/v1/admin/autopilot/beacons/{beacon_id}/signal-invites",
            "/v1/admin/autopilot/beacon-press-requests",
            "/v1/internal/beacon/notifications/emit-due",
        ):
            self.assertIn(route, ROUTING)
        self.assertIn('("beacon_signal_v1", true)', META)
        self.assertIn("SCHEMA_VERSION: u32 = 57", META)


if __name__ == "__main__":
    unittest.main()
