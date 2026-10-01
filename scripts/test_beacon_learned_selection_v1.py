#!/usr/bin/env python3
from __future__ import annotations

from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
EVALUATE = ROOT / "crates/crowdrelay-application/src/autopilot/evaluate.rs"
GI = ROOT / "crates/crowdrelay-application/src/autopilot/evaluate/growth_intelligence_context.rs"
BEACON = ROOT / "crates/crowdrelay-application/src/autopilot/evaluate/beacon_learning.rs"
CYCLE = ROOT / "crates/crowdrelay-application/src/autopilot/evaluate/causal_cycle.rs"
ENVELOPE = ROOT / "crates/crowdrelay-infra/src/autopilot/execution_dispatch.rs"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


class BeaconLearnedSelectionContract(unittest.TestCase):
    def test_one_causal_model_snapshot_is_shared_by_beacon_and_gi(self) -> None:
        evaluate = read(EVALUATE)
        gi = read(GI)
        cycle = read(CYCLE)

        self.assertIn("load_cycle_causal_model(&policies, &mut report)", evaluate)
        self.assertIn("loaded_model: &LoadedCausalModel", gi)
        self.assertNotIn("load_causal_model(self.workspace_id)", gi)
        self.assertEqual(cycle.count("load_causal_model(self.workspace_id)"), 2)
        self.assertIn("growth_intelligence_enabled", cycle)
        self.assertIn("beacon_enabled", cycle)
        self.assertIn("checkpoint_cycle_causal_model", evaluate)
        self.assertLess(
            evaluate.index("checkpoint_cycle_causal_model"),
            evaluate.index("for policy in policies.into_iter()"),
        )
        self.assertIn("save_brain_state_checkpoint", cycle)

    def test_shared_gi_model_keeps_nested_inputs_borrowed(self) -> None:
        gi = read(GI)

        self.assertIn("&causal_model.calibration.y30_interval", gi)
        self.assertIn("&causal_model.value_exchange", gi)

    def test_beacon_ranking_reads_the_same_causal_identity_as_its_envelope(self) -> None:
        beacon = read(BEACON)
        envelope = read(ENVELOPE)

        self.assertIn("predict_stats_with_treatment_for_target", beacon)
        self.assertIn("DispatchContext::default()", beacon)
        self.assertIn('format!("beacon:{beacon_id}")', beacon)

        self.assertIn("AutopilotActionPayload::RequestBeaconOutreach", envelope)
        self.assertIn("template_key.clone()", envelope)
        self.assertIn('format!("beacon:{beacon_id}")', envelope)
        self.assertIn("DispatchContext::default()", envelope)

    def test_learning_orders_only_candidates_the_domain_already_allowed(self) -> None:
        beacon = read(BEACON)

        candidate = beacon.index("beacon_candidate(snapshot, policy, now)?")
        rank = beacon.rindex("rank_beacon_candidate(")
        self.assertLess(candidate, rank)
        self.assertIn("already-eligible Beacon outreach", beacon)
        self.assertIn("may\n// reorder due asks; it must never make an ineligible Beacon eligible", beacon)

    def test_primary_fan_value_not_reply_or_click_proxy_drives_ordering(self) -> None:
        beacon = read(BEACON)

        self.assertIn("DecisionValue::from_stats", beacon)
        self.assertIn("rank_value_y30_fans", beacon)
        self.assertIn("expected_incremental_y30", beacon)
        self.assertIn(".with_harm_cost(&stats)", beacon)
        self.assertNotIn("beacon_outreach_reply_quality", beacon)
        self.assertNotIn("beacon_outreach_unique_visitors", beacon)

    def test_decision_records_what_learning_used(self) -> None:
        beacon = read(BEACON)

        for field in (
            "north_star_ranking",
            "template_id",
            "target_key",
            "rank_value_y30_fans",
            "expected_incremental_y30",
            "p_meaningful_effect",
            "estimation_regime",
            "sample_size",
            "uses_y30",
            "bridge_confidence",
            "harm_fans",
        ):
            self.assertIn(field, beacon)


if __name__ == "__main__":
    unittest.main()
