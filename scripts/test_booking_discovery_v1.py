"""Contract tests for venue/promoter discovery — the booking pipeline's supply.

The negotiation machinery is complete and starves: booking targets were
operator-upsert-only since 0033, which made zero venues a stable state rather
than a problem the agent could notice. Discovery gives the booking pipeline
what Phase 9 gave the pitcher.

Pinned here:
- Screening happens on write against a closed refusal set; the permanent
  refusals (inferred route, missing evidence, pay-to-play) fire before any
  policy threshold could save them.
- Dedupe is contact identity: one inbox is one prospect.
- Only a human's confirm click promotes an email route into a bookable,
  city-resolved target — and promotion never resets an existing relationship.
- The agent can ASK for supply: first_party_reversible, on the internal
  surface contract, with screening thresholds published to the adapter.
"""

from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
MIGRATION = ROOT / "migrations/0104_viryaos_booking_target_discovery.sql"
DOMAIN = ROOT / "crates/crowdrelay-domain/src/booking_discovery.rs"
MODEL = ROOT / "crates/crowdrelay-application/src/autopilot/model.rs"
CANDIDATE = ROOT / "crates/crowdrelay-application/src/autopilot/evaluate/outreach_supply.rs"
EVALUATE = ROOT / "crates/crowdrelay-application/src/autopilot/evaluate.rs"
SNAPSHOTS = ROOT / "crates/crowdrelay-infra/src/autopilot/operations/snapshots.rs"
INGRESS = (
    ROOT / "crates/crowdrelay-infra/src/autopilot/operations/ingress/booking_discovery.rs"
)
EXECUTION = ROOT / "crates/crowdrelay-infra/src/autopilot/execution.rs"
ACTIONS_EXECUTION = ROOT / "crates/crowdrelay-infra/src/autopilot/actions_execution.rs"
API = ROOT / "crates/crowdrelay-api/src/autopilot/booking_discovery.rs"
ROUTING = ROOT / "crates/crowdrelay-api/src/routing.rs"
OPENAPI = ROOT / "openapi/openapi.yaml"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


class BookingDiscoveryContract(unittest.TestCase):
    def setUp(self) -> None:
        self.sql = "\n".join(
            line.split("--", 1)[0] for line in read(MIGRATION).splitlines()
        )
        self.domain = read(DOMAIN)

    def test_candidates_are_rows_and_refusals_are_durable_knowledge(self) -> None:
        self.assertIn("CREATE TABLE viryaos_booking_candidates", self.sql)
        self.assertIn("status text NOT NULL DEFAULT 'admitted'", self.sql)
        self.assertIn("'route_inferred', 'evidence_missing', 'paid_to_apply', 'poor_fit'", self.sql)

    def test_dedupe_is_contact_identity(self) -> None:
        # The same inbox found through two sources is one prospect.
        # Expression dedupe requires a unique index, not a table constraint.
        self.assertIn(
            "CREATE UNIQUE INDEX viryaos_booking_candidates_route_identity_uq",
            self.sql,
        )
        self.assertIn("ON CONFLICT (workspace_id, route_kind, lower(btrim(route_value))) DO NOTHING", read(INGRESS))

    def test_permanent_refusals_fire_before_the_policy_floor(self) -> None:
        rule = self.domain.split("pub fn screen_candidate", 1)[1].split("\n}", 1)[0]
        order = [
            rule.find("RouteInferred"),
            rule.find("PaidToApply"),
            rule.find("EvidenceMissing"),
            rule.find("PoorFit"),
        ]
        self.assertEqual(order, sorted(order), "permanent refusals must fire first")
        self.assertIn("pub const fn is_permanent(self)", self.domain)
        self.assertIn("!matches!(self, Self::PoorFit)", self.domain)

    def test_a_candidate_is_not_a_target_confirmation_promotes_once(self) -> None:
        block = read(INGRESS).split("async fn confirm_booking_candidate", 1)[1].split("\n    async fn", 1)[0]
        self.assertIn("status = 'admitted'", block)
        self.assertIn('route_kind != "email"', block)
        self.assertIn("FROM cities WHERE slug = $1", block)
        self.assertIn("SET status = 'promoted', promoted_at = now(), booking_target_id = $3", block)
        # An existing relationship is linked, not reset.
        self.assertIn("ON CONFLICT (workspace_id, city_id, contact_email) DO NOTHING", block)
        self.assertIn("Promotion never resets anything.", read(INGRESS))

    def test_the_agent_can_ask_for_supply(self) -> None:
        model = read(MODEL)
        self.assertIn("RequestBookingTargetDiscovery", model)
        arm = model.split("RequestBookingTargetDiscovery { .. } => ", 1)[1][:80]
        self.assertIn('"booking.target_discovery.request"', arm)
        # First-party: reads published data, contacts nobody, buys nothing.
        # The variant sits inside the first-party arm of the class match; the
        # next ActionClass line after it must still be FirstPartyReversible.
        # Scan forward from the variant to the class that arm resolves to; the
        # first ActionClass reached must be FirstPartyReversible.
        seg = model.split("Self::RequestBookingTargetDiscovery { .. }", 1)[1]
        first_class = seg[seg.index("ActionClass::"):]
        self.assertTrue(
            first_class.startswith("ActionClass::FirstPartyReversible"),
            first_class[:60],
        )

    def test_the_request_publishes_its_screening_contract_to_the_adapter(self) -> None:
        emission = read(ACTIONS_EXECUTION).split(
            '"crowdrelay.booking.target_discovery_requested"', 1
        )[1][:2500]
        self.assertIn('"callback_path": "/v1/internal/autopilot/booking-discovery/candidates"', emission)
        self.assertIn("minimum_fit_basis_points", emission)
        self.assertIn("pay_to_apply_must_be_reported_as_such", emission)

    def test_executor_capability_mapping_closed_loop(self) -> None:
        execution = read(EXECUTION)
        self.assertIn('AutopilotActionPayload::RequestBookingTargetDiscovery { .. } => "booking.discovery"', execution)
        self.assertIn('"crowdrelay.booking.target_discovery_requested" => "booking.discovery"', execution)

    def test_supply_snapshot_reads_targets_and_cooldown_clock(self) -> None:
        sql = read(SNAPSHOTS).split(
            "pub(in crate::autopilot) async fn load_booking_supply_snapshot", 1
        )[1]
        self.assertIn("target.active", sql)
        self.assertIn("target.accepts_booking", sql)
        self.assertIn("request_booking_target_discovery", sql)

    def test_ingestion_surfaces_live_on_both_authority_surfaces(self) -> None:
        routing = read(ROUTING)
        self.assertIn("/v1/internal/autopilot/booking-discovery/candidates", routing)
        self.assertIn("/v1/admin/autopilot/booking-discovery/candidates/{candidate_id}/confirm", routing)
        handler = read(API)
        self.assertIn("commerce_authorized", handler)
        self.assertIn("deny_unknown_fields", handler)

    def test_openapi_documents_all_three_routes(self) -> None:
        openapi = read(OPENAPI)
        for marker in (
            "/admin/autopilot/booking-discovery/candidates:",
            "/admin/autopilot/booking-discovery/candidates/{candidate_id}/confirm:",
            "/internal/autopilot/booking-discovery/candidates:",
            "ingestAutopilotBookingCandidates",
            "confirmAutopilotBookingCandidate",
            "enum: [venue, promoter, festival]",
        ):
            self.assertIn(marker, openapi)


if __name__ == "__main__":
    unittest.main()
