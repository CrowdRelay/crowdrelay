#!/usr/bin/env python3
"""The operator's two show writes — the bill and the counterparty — reuse
the staff/admin handlers under the control-plane prefix.

`PUT /v1/control-plane/events/{event_slug}/acts` exists because `event_acts`
is the crossbill step's input and nothing operator-facing could write it:
the staff surface belongs to the band's own app, the admin surface to
platform operators, and neither credential belongs on the gig page. The
counterparty endpoint is the same shape — the T+7 report's delivery address
was settable only through staff/admin before.

This gate pins:

- both routes are registered on the control-plane router AND bound by
  `is_control_plane_management_path` — registration alone leaves the path
  reachable without authentication (the gate is also the privileged
  predicate), a failure mode this family has shipped before;
- both routes mount the shared `events::` handlers — a copied handler would
  drift from the staff surface's validation the day either changes;
- the bill route overrides the router-wide 8 KiB body limit with the same
  16 KiB the staff surface allows — a 32-act bill with ticket URLs does not
  fit 8 KiB, and a route that rejects its handler's own contract is a trap;
- the timeline's crossbill acts carry `position` and `ticket_url`, not just
  slug/name — the control plane's bill editor prefills from that payload,
  and a replace-the-whole-bill write that could not read those fields would
  silently drop them on every save.
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ROUTER = ROOT / "crates/crowdrelay-api/src/control_plane.rs"
LIB = ROOT / "crates/crowdrelay-api/src/lib.rs"
TIMELINE = ROOT / "crates/crowdrelay-api/src/concert_qr/timeline.rs"
FACTS = ROOT / "crates/crowdrelay-api/src/concert_qr/timeline_facts.rs"


class ControlPlaneShowWritesContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.router = ROUTER.read_text(encoding="utf-8")
        cls.lib = re.sub(r"\s+", " ", LIB.read_text(encoding="utf-8"))
        cls.timeline = TIMELINE.read_text(encoding="utf-8")
        cls.facts = FACTS.read_text(encoding="utf-8")

    def test_acts_route_is_registered_on_the_shared_handler(self) -> None:
        self.assertIn(
            '"/v1/control-plane/events/{event_slug}/acts"', self.router
        )
        self.assertIn("put(crate::events::replace_event_acts)", self.router)

    def test_counterparty_route_is_registered_on_the_shared_handler(self) -> None:
        self.assertIn(
            '"/v1/control-plane/events/{event_slug}/counterparty"', self.router
        )
        self.assertIn("put(crate::events::set_event_counterparty)", self.router)

    def test_the_management_path_predicate_covers_both(self) -> None:
        # The predicate is also the privileged-computation input: a control-
        # plane path missing here is served unauthenticated, not refused.
        self.assertIn(
            'one_segment_with_suffix(path, "/v1/control-plane/events/", "/acts")',
            self.lib,
        )
        self.assertIn(
            'one_segment_with_suffix(path, "/v1/control-plane/events/", "/counterparty")',
            self.lib,
        )

    def test_the_bill_route_sized_its_body_limit(self) -> None:
        declared = re.search(
            r"const MAX_EVENT_BILL_BODY_BYTES: usize = (\d+) \* 1024;",
            self.router,
        )
        self.assertIsNotNone(
            declared, "bill body limit must be a named KiB constant for review"
        )
        self.assertGreaterEqual(int(declared.group(1)), 16)
        self.assertLessEqual(int(declared.group(1)), 512)
        self.assertIn(
            "DefaultBodyLimit::max(MAX_EVENT_BILL_BODY_BYTES)", self.router
        )

    def test_the_timeline_acts_payload_round_trips_the_bill(self) -> None:
        # The editor replaces the whole bill; its prefill must carry every
        # field the write accepts or a save silently drops them.
        self.assertIn("position", self.facts)
        self.assertIn("ticket_url", self.facts)
        self.assertIn('"position": a.position', self.timeline)
        self.assertIn('"ticket_url": a.ticket_url', self.timeline)


if __name__ == "__main__":
    unittest.main()
