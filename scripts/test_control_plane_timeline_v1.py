#!/usr/bin/env python3
"""The show timeline answers "what state is Friday in" — nine steps, T-21
through T+7, each backed by the artifact that records it.

`GET /v1/control-plane/events/{event_slug}/timeline` composes per-show state
from a dozen existing artifact tables. This gate pins what the gig page
relies on:

- the route exists and stays on the control-plane authority boundary —
  never admin/staff, because the page must not grow admin privileges to
  render a ladder. The boundary is enforced twice: the router registration
  AND `is_control_plane_management_path`, without which the management
  credential check falls through for unlisted paths;
- every query is workspace-scoped — `workspace_id` is the whole of tenant
  isolation, and a timeline that dropped it on any one of its dozen queries
  would leak another tenant's night;
- only `published`/`completed` events resolve — drafts have no ladder, and
  a played show must keep resolving so T+1/T+3/T+7 still have a page;
- the nine steps exist in time order with their anchors — reordering them
  is exactly the regression the page is built to prevent;
- owners resolve through `viryaos_team_assignments.source_id` (both
  assignment shapes key the event there) joined to `workspace_members` —
  a step's owner is a display name, never an id the page cannot render,
  and the column is nullable so the row type must be `Option`;
- the brain's own numbers, not re-derived ones: pace capacity is the
  snapshot's `COALESCE(ticket_sale.capacity, admission.capacity)`, and a
  pending harvest request is the supply snapshot's exact predicate —
  open, or succeeded-at-request without a terminal execution report;
- no campaign tokens, signing keys, or per-fan rows reach the shape — the
  scan step carries counts and readiness, not credentials.
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "crates/crowdrelay-api/src/concert_qr/timeline.rs"
ROUTER = ROOT / "crates/crowdrelay-api/src/control_plane.rs"
PARENT = ROOT / "crates/crowdrelay-api/src/concert_qr.rs"
LIB = ROOT / "crates/crowdrelay-api/src/lib.rs"

STEPS = (
    "announced",
    "sales_pace",
    "bands_posting",
    "nearby_fans",
    "capture_plan",
    "the_scan",
    "recall",
    "harvest",
    "the_numbers",
)

ANCHORS = ("T-21", "T-14", "T-7", "T-2", "T-0", "T-0", "T+1", "T+3", "T+7")


class ControlPlaneTimelineContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.source = SOURCE.read_text(encoding="utf-8")
        cls.router = ROUTER.read_text(encoding="utf-8")
        cls.parent = PARENT.read_text(encoding="utf-8")
        cls.lib = re.sub(r"\s+", " ", LIB.read_text(encoding="utf-8"))

    def test_route_is_registered(self) -> None:
        self.assertIn(
            '"/v1/control-plane/events/{event_slug}/timeline"', self.router
        )
        self.assertIn("concert_qr::control_plane_event_timeline", self.router)

    def test_the_management_path_predicate_covers_it(self) -> None:
        # Route registration alone does not bind the credential: paths under
        # /v1/control-plane/ that the management predicate does not list fall
        # through the privileged-namespace check. This pair is what keeps the
        # timeline behind the ControlPlane bearer at the app layer.
        self.assertIn(
            'one_segment_with_suffix(path, "/v1/control-plane/events/", "/timeline")',
            self.lib,
        )

    def test_the_chunk_is_included(self) -> None:
        self.assertIn('include!("concert_qr/timeline.rs");', self.parent)
        self.assertIn('include!("concert_qr/timeline_tests.rs");', self.parent)

    def test_every_query_is_workspace_scoped(self) -> None:
        queries = re.findall(r"sqlx::query_as", self.source)
        self.assertGreaterEqual(
            len(queries),
            12,
            "the timeline fans out to a dozen artifacts — a dropped query "
            "means a step lost its evidence",
        )
        bodies = re.findall(r'r#"(.*?)"#', self.source, re.S)
        sql_bodies = [body for body in bodies if re.search(r"\bFROM\b", body)]
        self.assertGreaterEqual(
            len(sql_bodies), 12, "expected the dozen SQL bodies to be found"
        )
        for body in sql_bodies:
            self.assertRegex(
                body,
                r"workspace_id\s*=",
                f"unscoped query in timeline: {body[:80]!r}",
            )

    def test_only_live_events_resolve(self) -> None:
        self.assertIn("status IN ('published','completed')", self.source)

    def test_the_ladder_is_complete_and_ordered(self) -> None:
        # The order the steps are pushed is the order the page renders —
        # assert the `step("key"` calls themselves appear in time order,
        # not merely that each key exists somewhere in the file.
        positions = []
        for key in STEPS:
            match = re.search(rf'step\(\s*"{key}"', self.source)
            self.assertIsNotNone(match, f"step {key} missing")
            positions.append(match.start())
        self.assertEqual(
            positions, sorted(positions), "the ladder left time order"
        )
        for anchor in ANCHORS:
            self.assertIn(f'"{anchor}"', self.source)

    def test_owner_join_uses_the_handoff_index(self) -> None:
        self.assertIn("viryaos_team_assignments", self.source)
        self.assertIn("assignment.source_id = $2", self.source)
        self.assertIn("assignment.action_id", self.source)
        self.assertIn("workspace_members", self.source)
        # display_name is nullable on workspace_members — a non-Option row
        # type turns one unnamed assignee into a 503 for the whole page.
        self.assertIn("display_name: Option<String>", self.source)

    def test_the_numbers_match_the_brains_predicates(self) -> None:
        self.assertIn(
            "COALESCE(ticket_sale.capacity, admission.capacity)", self.source
        )
        self.assertIn("viryaos_autopilot_action_emissions", self.source)
        self.assertIn("report.status IN ('succeeded','failed')", self.source)
        self.assertIn("phase = 'announcement'", self.source)

    def test_no_credentials_or_fan_rows_in_the_shape(self) -> None:
        for leaked in ("token", "signing_key", "fan_id", "email"):
            self.assertNotIn(leaked, self.source)
        # The scan step counts the room; it never returns who is in it.
        self.assertIn("count(*)::bigint FROM concert_checkins", self.source)

    def test_absence_is_a_state_not_a_zero(self) -> None:
        # Steps report `due` once their window opens without evidence — the
        # vocabulary the page renders. A `pending`/`unknown` placeholder
        # state would mean the ladder learned to shrug.
        self.assertIn('"due"', self.source)
        self.assertIn('"waiting"', self.source)
        self.assertIn('"skipped"', self.source)


if __name__ == "__main__":
    unittest.main()
