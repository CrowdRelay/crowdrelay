"""Contract tests for the scene-node invite — the acquisition ask.

Beacons exist to reach rooms the band cannot, the invite machinery already
works, and this is what connects them: a verified scene node is asked to run
invite codes for their own city's show. Every signup those codes produce is
attributed and consented by construction — the codes are ours.

The properties that keep it safe:
- The ask is third-party contact and carries its class with it.
- Warmth gates it: a name on a list is not a partner.
- One batch per beacon per show, cooled down.
- The executor delivers the ask; CrowdRelay issues the codes. Nothing here
  invents signups.
"""

from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
DOMAIN = ROOT / "crates/crowdrelay-domain/src/beacons.rs"
CANDIDATE = ROOT / "crates/crowdrelay-application/src/autopilot/evaluate/beacons.rs"
EVALUATE = ROOT / "crates/crowdrelay-application/src/autopilot/evaluate.rs"
MODEL = ROOT / "crates/crowdrelay-application/src/autopilot/model.rs"
PORTS = ROOT / "crates/crowdrelay-application/src/autopilot/ports.rs"
SNAPSHOTS = ROOT / "crates/crowdrelay-infra/src/autopilot/operations/snapshots.rs"
EXECUTION = ROOT / "crates/crowdrelay-infra/src/autopilot/execution.rs"
ACTIONS_EXECUTION = ROOT / "crates/crowdrelay-infra/src/autopilot/actions_execution.rs"
CONTRACT_DOC = ROOT / "n8n/viryaos-executor-contract.md"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


class BeaconInviteContract(unittest.TestCase):
    def setUp(self) -> None:
        self.domain = read(DOMAIN)

    def test_the_ask_is_third_party(self) -> None:
        # It is a request to somebody else's community, not a message to ours;
        # the class ceiling, envelope and posture all gate on that.
        model = read(MODEL)
        block = model.split("RequestBeaconInviteBatch {", 1)[1]
        third_party = block.split("ActionClass::ThirdParty", 1)[0]
        self.assertIn("Self::RequestBeaconInviteBatch { .. }", third_party)

    def test_warmth_gates_the_ask(self) -> None:
        rule = self.domain.split("pub fn evaluate_beacon_invite_batch", 1)[1].split("\n}", 1)[0]
        self.assertIn("minimum_relationship_score", rule)
        self.assertIn("LowWarmth", rule)
        self.assertIn("Ineligible", rule)

    def test_the_window_has_both_edges_and_a_cooldown(self) -> None:
        rule = self.domain.split("pub fn evaluate_beacon_invite_batch", 1)[1].split("\n}", 1)[0]
        for reason in ("NotDue", "WindowClosed", "OnCooldown"):
            self.assertIn(reason, rule)
        policy = self.domain.split("pub struct BeaconInvitePolicy", 1)[1].split("\n}", 1)[0]
        self.assertIn("invite_lead_days", policy)
        self.assertIn("invite_cooldown_days", policy)
        self.assertIn("max_invites_per_batch", policy)
        # Old policy rows must still parse: knobs default.
        self.assertIn("#[serde(default)]", self.domain.split("pub struct BeaconInvitePolicy", 1)[0])

    def test_codes_are_ours_so_signups_are_attributable(self) -> None:
        emission = read(ACTIONS_EXECUTION).split(
            "crowdrelay.beacon.invite_batch_requested", 1
        )[1][:2000]
        self.assertIn('"codes_issued_by_crowdrelay": true', emission)
        self.assertIn('"never_purchase_or_bot_invites": true', emission)
        self.assertIn('"only_their_own_community": true', emission)

    def test_executor_capability_is_advertised_and_mapped(self) -> None:
        execution = read(EXECUTION)
        self.assertIn('AutopilotActionPayload::RequestBeaconInviteBatch { .. } => "beacon.invite_batch"', execution)
        self.assertIn("| AutopilotActionPayload::RequestBeaconInviteBatch { .. }", execution)
        doc = read(CONTRACT_DOC)
        self.assertIn("beacon.invite_batch", doc)

    def test_snapshot_loader_scopes_to_their_own_city(self) -> None:
        sql = read(SNAPSHOTS).split(
            "pub(in crate::autopilot) async fn load_beacon_invite_snapshots", 1
        )[1]
        self.assertIn("beacon.city_id IS NULL OR beacon.city_id = event.city_id", sql)
        self.assertIn("'beacon.invite_batch.request'", sql)
        self.assertIn("beacon.contact_email IS NOT NULL", sql)

    def test_one_ask_per_beacon_per_show_for_ever(self) -> None:
        candidate = read(CANDIDATE).split("fn beacon_invite_candidate", 1)[1]
        self.assertIn('"action:beacon-invite:{}:{}"', candidate)
        self.assertIn("snapshot.beacon_id", candidate)
        self.assertIn("snapshot.event_id", candidate)

    def test_evaluation_loop_consumes_the_loader(self) -> None:
        loop = read(EVALUATE)
        self.assertIn("load_beacon_invite_snapshots", loop)
        self.assertIn("beacon_invite_candidate(snapshot, &policy, now)", loop)


if __name__ == "__main__":
    unittest.main()
