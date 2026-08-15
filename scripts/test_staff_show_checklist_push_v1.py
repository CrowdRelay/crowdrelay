#!/usr/bin/env python3
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]

def text(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")

class StaffShowChecklistPushV1Contract(unittest.TestCase):
    def test_schema_models_staff_as_distinct_push_audience(self):
        migration = text("migrations/0055_staff_show_checklist_push.sql")
        self.assertIn("audience_kind = 'staff'", migration)
        self.assertIn("principal_hash", migration)
        self.assertIn("UNIQUE (workspace_id, installation_id, transport, audience_kind)", migration)
        self.assertIn("'show_checklist'", migration)

    def test_scheduler_emits_exactly_week_and_two_day_staff_push_phases(self):
        reminders = text("crates/crowdrelay-worker/src/reminders.rs")
        self.assertIn("interval '6 days 18 hours'", reminders)
        self.assertIn("interval '7 days'", reminders)
        self.assertIn("interval '42 hours'", reminders)
        self.assertIn("interval '48 hours'", reminders)
        self.assertIn("'/staff/checklist?event='", reminders)
        self.assertIn("endpoint.audience_kind = 'staff'", reminders)

    def test_staff_routes_share_canonical_checklist_and_push_transport(self):
        routing = text("crates/crowdrelay-api/src/routing.rs")
        for route in (
            '/v1/staff/ecosystem/checklists/{event_slug}',
            '/v1/staff/ecosystem/checklists/{event_slug}/{item_key}',
            '/v1/staff/push/endpoints',
            '/v1/staff/push/endpoints/disable',
        ):
            self.assertIn(route, routing)
        control = text("crates/crowdrelay-api/src/ecosystem/control_plane.rs")
        for key in (
            'laptop_charged_packed', 'setlist_ready', 'merch_packed',
            'rack_cables_instruments_packed', 'camera_handoff_ready',
            'stage_outfit_packed', 'wireless_checked',
        ):
            self.assertIn(key, control)

    def test_staff_session_revocation_invalidates_staff_push(self):
        sessions = text("crates/crowdrelay-api/src/staff_sessions.rs")
        self.assertIn("staff_session_revoked", sessions)
        self.assertIn("endpoint.principal_hash = revoked.token_hash", sessions)

if __name__ == "__main__":
    unittest.main()
