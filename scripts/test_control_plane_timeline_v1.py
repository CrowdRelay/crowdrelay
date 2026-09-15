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
FACTS = ROOT / "crates/crowdrelay-api/src/concert_qr/timeline_facts.rs"
SCAN = ROOT / "crates/crowdrelay-api/src/concert_qr/scan_view.rs"
REPORT = ROOT / "crates/crowdrelay-api/src/concert_qr/report_view.rs"
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
        # The module is two chunks — `timeline.rs` owns the response shape
        # and step builder, `timeline_facts.rs` owns the row types and the
        # workspace-scoped fan-out. The contract pins the module's behavior,
        # so it reads both.
        cls.source = SOURCE.read_text(encoding="utf-8") + FACTS.read_text(
            encoding="utf-8"
        )
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
        self.assertIn('include!("concert_qr/timeline_facts.rs");', self.parent)
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

    def test_the_placement_table_holds(self) -> None:
        # UX-2.5 — every artifact sits at its anchor with no new top-level
        # destination: the crossbill on T-21 (cap is the consent's own), the
        # recap campaign on T+1, venue knowledge on the show itself.
        self.assertIn('"crossbill"', self.source)
        self.assertIn("amplification_consents", self.source)
        self.assertIn("event_crossbill", self.source)
        self.assertIn("to_workspace_id", self.source)
        self.assertIn('"campaign"', self.source)
        self.assertIn("content->>'lever' = 'post_show_recap'", self.source)
        self.assertIn("viryaos_beacon_campaigns", self.source)
        self.assertIn("venue_knowledge", self.source)
        self.assertIn("venue_address", self.source)

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


class ControlPlaneScanContract(unittest.TestCase):
    # The door view is where the token-bearing URL is allowed to exist —
    # its own route, its own credential scope, never smuggled into the
    # timeline or list shapes the page polls broadly.
    @classmethod
    def setUpClass(cls) -> None:
        cls.source = SCAN.read_text(encoding="utf-8")
        cls.router = ROUTER.read_text(encoding="utf-8")
        cls.parent = PARENT.read_text(encoding="utf-8")
        cls.lib = re.sub(r"\s+", " ", LIB.read_text(encoding="utf-8"))

    def test_scan_route_is_registered(self) -> None:
        self.assertIn('"/v1/control-plane/events/{event_slug}/scan"', self.router)
        self.assertIn("concert_qr::control_plane_event_scan", self.router)

    def test_scan_path_is_under_the_management_credential(self) -> None:
        self.assertIn(
            'one_segment_with_suffix(path, "/v1/control-plane/events/", "/scan")',
            self.lib,
        )

    def test_scan_chunk_is_included(self) -> None:
        self.assertIn('include!("concert_qr/scan_view.rs");', self.parent)

    def test_scan_queries_are_workspace_scoped(self) -> None:
        bodies = [
            body
            for body in re.findall(r'r#"(.*?)"#', self.source, re.S)
            if re.search(r"\bFROM\b", body)
        ]
        self.assertGreaterEqual(len(bodies), 2, "expected the event + campaign reads")
        for body in bodies:
            self.assertRegex(body, r"workspace_id\s*=", f"unscoped scan query: {body[:80]!r}")

    def test_scan_only_signs_a_live_campaign(self) -> None:
        # The pick must match the staff print tool's semantics — active,
        # unrevoked, still inside its validity window — or the page hands
        # the door a token the door will reject.
        self.assertIn("row.active", self.source)
        self.assertIn("row.revoked_at.is_none()", self.source)
        self.assertIn("row.valid_until > OffsetDateTime::now_utc()", self.source)

    def test_scan_no_campaign_is_null_not_fabricated(self) -> None:
        self.assertIn("checkin_url: Option<String>", self.source)
        # An unresolvable show is a 404; a show with no live campaign is a
        # null URL — both are facts the phone page renders, not 500s.
        self.assertIn("Problem::not_found", self.source)
        # But a live campaign that cannot be signed is a fault — returning
        # null there would have the door create a duplicate and hit the
        # same wall. Siblings return 503 on a missing key; so does this.
        self.assertIn("Problem::service_unavailable", self.source)

    def test_scan_reports_the_windows_open_edge(self) -> None:
        # valid_until keeps the pick honest; valid_from is what makes a
        # pre-window QR say "goes live at" instead of silently dead.
        self.assertIn("campaign.valid_from", self.source)
        self.assertIn("valid_from: Option<String>", self.source)

    def test_scan_url_matches_the_print_tool(self) -> None:
        # The fragment-bound credential belongs on the fan site's live
        # page — the same /pl/live/{slug} the staff print tool emits —
        # assembled from the configured site base, never a hardcoded host.
        self.assertIn("public_site_base_url", self.source)
        self.assertIn('"pl/live"', self.source)
        self.assertRegex(
            self.source,
            r'"#checkin=\{token\}"|#checkin=\{token\}',
            "the URL must carry the token in the fragment",
        )


class ControlPlaneReportContract(unittest.TestCase):
    # The T+7 counterparty artifact: the page renders the mailed payload
    # when it exists and a live composition only before it does — never a
    # re-derived "report" disagreeing with what the promoter actually got.
    @classmethod
    def setUpClass(cls) -> None:
        cls.source = REPORT.read_text(encoding="utf-8")
        cls.router = ROUTER.read_text(encoding="utf-8")
        cls.parent = PARENT.read_text(encoding="utf-8")
        cls.lib = re.sub(r"\s+", " ", LIB.read_text(encoding="utf-8"))

    def test_report_route_is_registered(self) -> None:
        self.assertIn('"/v1/control-plane/events/{event_slug}/report"', self.router)
        self.assertIn("concert_qr::control_plane_event_report", self.router)

    def test_report_path_is_under_the_management_credential(self) -> None:
        self.assertIn(
            'one_segment_with_suffix(path, "/v1/control-plane/events/", "/report")',
            self.lib,
        )

    def test_report_chunk_is_included(self) -> None:
        self.assertIn('include!("concert_qr/report_view.rs");', self.parent)

    def test_report_queries_are_workspace_scoped(self) -> None:
        bodies = [
            body
            for body in re.findall(r'r#"(.*?)"#', self.source, re.S)
            if re.search(r"\bFROM\b", body)
        ]
        self.assertGreaterEqual(len(bodies), 6, "expected event + issued + evidence reads")
        for body in bodies:
            self.assertRegex(body, r"workspace_id\s*=", f"unscoped report query: {body[:80]!r}")

    def test_issued_artifact_wins_over_live_composition(self) -> None:
        # The outbox lookup must run BEFORE the preview branch — a report
        # that disagrees with the mailed artifact is worse than no report.
        self.assertIn("crowdrelay.show.post_show_report_due", self.source)
        self.assertIn("ORDER BY created_at DESC", self.source)
        lookup = self.source.find("post_show_report_due")
        preview = self.source.find("compose the preview")
        self.assertGreater(preview, lookup, "the issued check must precede the preview")

    def test_the_evidence_contract_survives_the_preview(self) -> None:
        for key in ("observed", "inferred", "evidence_gaps", "honesty_contract"):
            self.assertIn(f'"{key}"', self.source)
        # The four honesty rules travel with every render — the page shows
        # the same contract the email does.
        self.assertIn("never_sum_numbers_across_evidence_classes", self.source)
        self.assertIn("room_attendance_unverified", self.source)

    def test_issued_carries_delivery_status(self) -> None:
        # `issued_at` alone cannot distinguish "mailed" from "queued and died"
        # — the response must expose the outbox status so the page can say
        # "Sent" only once the mail actually left.
        self.assertIn("delivery_status", self.source)
        self.assertIn("delivered_at", self.source)

    def test_the_artifact_survives_terminal_retention(self) -> None:
        # The emitted payload is the ONLY copy of what the counterparty was
        # mailed — terminal outbox retention must exempt it or the page
        # silently reverts to a dishonest "preview" for a mailed report.
        retention = (
            ROOT / "crates/crowdrelay-worker/src/retention/steps.rs"
        ).read_text(encoding="utf-8")
        delete = re.search(
            r"delete_old_terminal_outbox_events.*?WITH candidates AS \(.*?event\.event_type <> '([^']+)'",
            retention,
            re.S,
        )
        self.assertIsNotNone(
            delete, "the terminal-outbox DELETE must exempt the report artifact"
        )
        self.assertEqual(delete.group(1), "crowdrelay.show.post_show_report_due")


if __name__ == "__main__":
    unittest.main()
