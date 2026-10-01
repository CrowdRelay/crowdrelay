#!/usr/bin/env python3
"""Every emitted outbox event type must be routed, internal, or named pending.

`materialize_deliveries_batch` fans every outbox event out to every active
webhook endpoint. The n8n bridge answers 422 for a type its route map does not
know, and 422 is a permanent failure — the delivery is marked `cancelled`,
never retried. `ops/edge/routes.json` is that route map, so any event type the
code emits but the map does not list is a delivery guaranteed to die.

Measured twice in production: seven `agent.content_requested` deliveries were
refused on 2026-09-13 (every press pitch ever drafted, gone), and
`autopilot.authority_demoted` refusals were dead on 2026-09-26. The audit that
produced this gate found 24 emitted types with no route — including real work
(`show_growth.requested`, `booking.target_discovery_requested`,
`terms_accepted`) that silently never ran.

Emissions arrive two ways, and both are extracted:

- Direct `INSERT INTO outbox_events` statements carry the type as a SQL
  literal.
- `emit_external_action*` / `emit_outward_action*` take a `&'static str`, and
  every one of them passes `executor_capability_for_event`, so the capability
  map's match-arm keys are the complete set of those types.

Classification of a type that has no route is a decision, not a detail:

- `INTERNAL_ONLY` — emitted for in-process readers (ops surfaces, the
  watchdog). Routing one would forward an internal signal to a workflow that
  has no business seeing it, so the gate fails if one ever appears in
  `routes.json`.
- `PENDING_ROUTE` — capability-shaped work awaiting its n8n workflow. Each
  entry is an admitted gap: the event emits, the bridge refuses it, and the
  work is lost until ops wires the route. The gate fails when one of these
  lands in `routes.json` so the entry is removed rather than left to rot.

A new emitted type failing here means: add its route (and the endpoint's
`event_types` entry) or declare it internal — do not let it 422.
"""
import json
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CRATES = ROOT / "crates"
ROUTES = ROOT / "ops/edge/routes.json"
CAPABILITIES = ROOT / "crates/crowdrelay-infra/src/autopilot/execution_capabilities.rs"

EVENT_TYPE = re.compile(r"^[a-z][a-z0-9_]*\.[a-z0-9_.]+$")

# In-process notifications. The operator brief, watchdog and attention feeds
# read `outbox_events` directly; nothing external may consume these, so a
# route for one is a leak, not a fix.
INTERNAL_ONLY: set[str] = {
    "crowdrelay.autopilot.authority_demoted",
    "crowdrelay.ops.reply_needs_human",
    "crowdrelay.beacon.coverage_submitted",
    "crowdrelay.beacon.press_request_created",
    "crowdrelay.beacon.press_request_resolved",
    "crowdrelay.beacon.release_delivery_confirmed",
    "crowdrelay.beacon.signal_engagement_recorded",
    "crowdrelay.beacon.signal_left",
    "crowdrelay.beacon.signal_state_changed",
}

# Capability-shaped work with no n8n workflow yet — the emit lives, the
# bridge has no route, the delivery dies `cancelled`. Each needs its route in
# `routes.json` (and the workflow behind it) before it delivers; an entry
# landing in the route map must leave this list.
PENDING_ROUTE: set[str] = {
    "amplification.campaign_due",
    "crowdrelay.beacon.invite_batch_requested",
    "crowdrelay.beacon.invite_delivery_requested",
    "crowdrelay.beacon.network_discovery_requested",
    "crowdrelay.beacon.outreach_requested",
    "crowdrelay.beacon.release_delivery_confirmation_requested",
    "crowdrelay.booking.target_discovery_requested",
    "crowdrelay.opportunity.counterparty_report_issued",
    "crowdrelay.opportunity.terms_accepted",
    "crowdrelay.opportunity.terms_countered",
    "crowdrelay.play.step_requested",
    "crowdrelay.playlist.placement_check_requested",
    "crowdrelay.promotion.budget_change_requested",
    "crowdrelay.release.editorial_pitch_escalated",
    "crowdrelay.release.editorial_pitch_parked",
    "crowdrelay.release.likely_listeners",
    "crowdrelay.release.r14_report_due",
    "crowdrelay.release.r3_report_due",
    "crowdrelay.show_growth.requested",
}


def emitted_event_types() -> set[str]:
    found: set[str] = set()
    for source_file in CRATES.rglob("*.rs"):
        path = str(source_file)
        if "/tests/" in path or path.endswith("_tests.rs"):
            continue
        source = source_file.read_text()
        # SQL literal inside an `INSERT INTO outbox_events` statement.
        for insert in re.finditer(r"INSERT INTO outbox_events", source):
            window = source[insert.start() : insert.start() + 900]
            end = window.find('";')
            if end > 0:
                window = window[:end]
            found |= {
                literal
                for literal in re.findall(r"'([a-z][a-z0-9_]*\.[a-z0-9_.]+)'", window)
                if EVENT_TYPE.match(literal)
            }
    # Every `emit_external_action*` type is a capability-map key.
    capability_source = CAPABILITIES.read_text()
    map_fn = re.split(
        r"\n(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn ",
        capability_source.split("fn executor_capability_for_event", 1)[1],
        maxsplit=1,
    )[0]
    found |= set(re.findall(r'"([a-z][a-z0-9_]*\.[a-z0-9_.]+)"\s*=>', map_fn))
    return found


def routed_event_types() -> set[str]:
    return set(json.loads(ROUTES.read_text()))


class OutboxRouteParity(unittest.TestCase):
    def test_every_routable_emit_has_a_route(self):
        missing = emitted_event_types() - routed_event_types() - INTERNAL_ONLY - PENDING_ROUTE
        self.assertEqual(
            missing,
            set(),
            f"emitted but unrouted and unclassified: {sorted(missing)}. Each "
            f"reaches the n8n bridge, is refused with 422 and dies `cancelled`. "
            f"Route it in ops/edge/routes.json, or classify it in this gate.",
        )

    def test_internal_events_stay_unrouted(self):
        leaked = INTERNAL_ONLY & routed_event_types()
        self.assertEqual(
            leaked,
            set(),
            f"{sorted(leaked)} are in-process signals; a route forwards them to "
            f"a workflow that must not see them.",
        )

    def test_pending_routes_are_still_pending(self):
        resolved = PENDING_ROUTE & routed_event_types()
        self.assertEqual(
            resolved,
            set(),
            f"{sorted(resolved)} now have routes — remove them from "
            f"PENDING_ROUTE so the list stays honest.",
        )

    def test_classification_lists_stay_real(self):
        emitted = emitted_event_types()
        stale_internal = INTERNAL_ONLY - emitted
        stale_pending = PENDING_ROUTE - emitted
        self.assertEqual(
            stale_internal,
            set(),
            f"{sorted(stale_internal)} are no longer emitted — drop them from "
            f"INTERNAL_ONLY.",
        )
        self.assertEqual(
            stale_pending,
            set(),
            f"{sorted(stale_pending)} are no longer emitted — drop them from "
            f"PENDING_ROUTE.",
        )

    def test_extraction_is_not_silently_empty(self):
        # If either reader stops matching, the assertions above pass vacuously.
        self.assertGreaterEqual(len(emitted_event_types()), 45)
        self.assertGreaterEqual(len(routed_event_types()), 50)


if __name__ == "__main__":
    unittest.main()
