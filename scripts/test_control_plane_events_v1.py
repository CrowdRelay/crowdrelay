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
- Only `published` events list — drafts must not reach a band-facing page.
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
        cls.source = SOURCE.read_text(encoding="utf-8")
        cls.router = ROUTER.read_text(encoding="utf-8")

    def test_route_is_registered(self) -> None:
        self.assertIn('"/v1/control-plane/events"', self.router)
        self.assertIn("concert_qr::control_plane_events", self.router)

    def test_upcoming_flag_uses_staff_boundary(self) -> None:
        self.assertIn("interval '36 hours') AS upcoming", self.source)

    def test_ordering_is_upcoming_then_past(self) -> None:
        self.assertIn("ORDER BY upcoming DESC", self.source)
        # Postgres resolves output aliases only as bare ORDER BY terms — a
        # SELECT alias inside an expression is looked up as an input column
        # and the query fails on first request. The CASE arms must repeat the
        # predicate, never reference `upcoming` bare.
        self.assertNotIn("CASE WHEN upcoming", self.source)
        self.assertNotIn("CASE WHEN NOT upcoming", self.source)
        self.assertIn(
            "CASE WHEN event.starts_at >= now() - interval '36 hours'", self.source
        )
        self.assertIn(
            "CASE WHEN event.starts_at < now() - interval '36 hours'", self.source
        )

    def test_published_only_and_bounded_lookback(self) -> None:
        self.assertIn("event.status = 'published'", self.source)
        self.assertIn("interval '90 days'", self.source)

    def test_workspace_scoped_including_checkin_join(self) -> None:
        self.assertIn("event.workspace_id = $1", self.source)
        self.assertIn("checkin.workspace_id = event.workspace_id", self.source)


if __name__ == "__main__":
    unittest.main()
