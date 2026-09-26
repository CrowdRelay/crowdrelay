#!/usr/bin/env python3
"""The control-plane show list answers "next up, then past" — its ordering
and filters are the contract the gig page renders against.

`GET /v1/control-plane/events` exists because the staff event-qr overview
carries campaign signing tokens the control-plane surface must never expose,
and the public events surface is the wrong authority boundary. This gate pins
the invariants the page relies on:

- `upcoming` is computed in SQL off the same now-36h boundary the staff view
  uses — a show stays "next up" through the night it is played.
- Ordering is upcoming-ascending then past-descending — the two `CASE WHEN`
  arms in ORDER BY are what make one list read "next up, then past". A plain
  `ORDER BY starts_at` buries next Friday under every past date.
- `draft`/`published`/`completed` events list — since 0329 a booked-but-
  unannounced show is a draft the operator must see to announce (the
  timeline endpoint resolves the same three statuses). `cancelled` still
  never reaches the page.
- Workspace scoping on both the event and the check-in join — tenant
  isolation is the whole of the WHERE clause.
- A bounded lookback — the page is a season, not an archive.
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "crates/crowdrelay-api/src/concert_qr.rs"
ROUTER = ROOT / "crates/crowdrelay-api/src/control_plane.rs"


class ControlPlaneEventsContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        # concert_qr.rs pulls its sections in with include!(); the events list
        # query lives in concert_qr/events_list.rs since the Shows pages
        # moved it, so the gate reads the module as the compiler does.
        main = SOURCE.read_text(encoding="utf-8")
        included = [
            (SOURCE.parent / name).read_text(encoding="utf-8")
            for name in re.findall(r'include!\("([^"]+)"\)', main)
        ]
        cls.source = "\n".join([main, *included])
        cls.router = ROUTER.read_text(encoding="utf-8")

    def test_route_is_registered(self) -> None:
        self.assertIn('"/v1/control-plane/events"', self.router)
        self.assertIn("concert_qr::control_plane_events", self.router)

    def test_upcoming_flag_uses_staff_boundary(self) -> None:
        self.assertIn("interval '36 hours') AS upcoming", self.source)

    def test_ordering_is_upcoming_then_past(self) -> None:
        # The flag is computed once in the `listed` CTE, so the outer ORDER BY
        # reads it as an input column (`listed.upcoming`) — valid inside the
        # CASE arms. A bare `upcoming` there would be a SELECT output alias,
        # which Postgres does not resolve inside an expression: the query
        # would fail on first request.
        self.assertIn("ORDER BY listed.upcoming DESC", self.source)
        self.assertNotIn("CASE WHEN upcoming", self.source)
        self.assertNotIn("CASE WHEN NOT upcoming", self.source)
        self.assertIn(
            "CASE WHEN listed.upcoming THEN listed.starts_at END,", self.source
        )
        self.assertIn(
            "CASE WHEN NOT listed.upcoming THEN listed.starts_at END DESC",
            self.source,
        )

    def test_terminal_shows_stay_listed_with_bounded_lookback(self) -> None:
        # `completed` belongs here: the list's past section is the only
        # navigation path to the T+1/T+3/T+7 steps the timeline serves.
        # `draft` joined in 0329 — a booked-but-unannounced show is exactly
        # the row the operator needs listed so they can announce it.
        # `cancelled` still never reaches the page.
        self.assertIn(
            "event.status IN ('draft','published','completed')", self.source
        )
        self.assertIn("interval '90 days'", self.source)

    def test_workspace_scoped_including_checkin_join(self) -> None:
        self.assertIn("event.workspace_id = $1", self.source)
        # The check-in join scopes through the CTE row, which carries the
        # event's own workspace_id.
        self.assertIn("checkin.workspace_id = listed.workspace_id", self.source)


if __name__ == "__main__":
    unittest.main()
