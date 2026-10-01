#!/usr/bin/env python3
"""Fan outcomes are the fans traced to the action, never the workspace's.

Until 2026-09-27 the five fan measurements counted `fans` created in the
action's window. Every dispatch was credited with every arrival from any
cause, and overlapping dispatches with the same ones: 145 fan-observations
against 23 fans ever, and the brain's worker ranking was learned from that.
The query shape that did it was one line — `SELECT COUNT(*) FROM fans WHERE
created_at in the window` — and nothing would stop it from coming back.

This pins three things, each of which must match before it is compared (a
check that matches nothing is not a check):

1. `AutopilotMeasurementKind::counts_attributed_fans` lists exactly the five
   fan kinds, and the observation match arm that routes to
   `attributed_fans::observe_attributed_fans` lists the same five.
2. No other arm in `observation.rs` names one of those kinds.
3. Every count in `attributed_fans.rs` is scoped to the action's lineage
   (`action_id IN (SELECT action_id FROM lineage)`) and none reads
   `FROM fans` — fans are only ever joined to a conversion.
"""
from __future__ import annotations

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PORTS = ROOT / "crates/crowdrelay-application/src/autopilot/measurement_ports.rs"
OBSERVATION = ROOT / "crates/crowdrelay-infra/src/autopilot/measurement/observation.rs"
ATTRIBUTED = (
    ROOT / "crates/crowdrelay-infra/src/autopilot/measurement/observation/attributed_fans.rs"
)
CHANNEL_YIELD = (
    ROOT
    / "crates/crowdrelay-infra/src/autopilot/operations/growth_intelligence/channel_yield.rs"
)

EXPECTED = {
    "AgentRunFanGrowth14d",
    "AgentRunFanGrowth3d",
    "IncrementalFanGrowth14d",
    "IncrementalFanGrowth3d",
    "DurableFanGrowth30d",
}


def attributed_kinds() -> set[str]:
    source = PORTS.read_text()
    match = re.search(
        r"pub const fn counts_attributed_fans\(self\) -> bool \{\s*matches!\(\s*self,(?P<body>[^)]*)\)",
        source,
    )
    if match is None:
        raise AssertionError("counts_attributed_fans not found in measurement_ports.rs")
    return set(re.findall(r"Self::(\w+)", match.group("body")))


def routed_kinds() -> set[str]:
    source = OBSERVATION.read_text()
    match = re.search(
        r"(?P<arm>(?:\|?\s*AutopilotMeasurementKind::\w+\s*)+)=>\s*\{\s*attributed_fans::observe_attributed_fans",
        source,
    )
    if match is None:
        raise AssertionError("no observation arm routes to attributed_fans::observe_attributed_fans")
    return set(re.findall(r"AutopilotMeasurementKind::(\w+)", match.group("arm")))


def sql_constants(source: str) -> dict[str, str]:
    return {
        name: body
        for name, body in re.findall(r'const (\w+): &str = r#"(.*?)"#;', source, re.DOTALL)
    }


class AttributedFanOutcomes(unittest.TestCase):
    def test_the_attributed_kinds_are_the_five_fan_kinds(self) -> None:
        self.assertEqual(attributed_kinds(), EXPECTED)

    def test_the_observation_routes_exactly_those_kinds(self) -> None:
        self.assertEqual(routed_kinds(), EXPECTED)

    def test_no_other_arm_names_a_fan_kind(self) -> None:
        source = OBSERVATION.read_text()
        for kind in EXPECTED:
            occurrences = len(re.findall(rf"AutopilotMeasurementKind::{kind}\b", source))
            self.assertEqual(
                occurrences,
                1,
                f"{kind} appears {occurrences} times in observation.rs; only the "
                "attributed arm may name it",
            )

    def test_every_count_is_scoped_to_the_lineage(self) -> None:
        constants = sql_constants(ATTRIBUTED.read_text())
        counts = {name: sql for name, sql in constants.items() if "COUNT(" in sql}
        self.assertGreaterEqual(len(counts), 2, f"expected the two count queries, found {sorted(constants)}")
        for name, sql in counts.items():
            self.assertIn("FROM fan_provenance_events", sql, name)
            self.assertIn("action_id IN (SELECT action_id FROM lineage)", sql, name)

    def test_fans_are_only_joined_to_a_conversion(self) -> None:
        constants = sql_constants(ATTRIBUTED.read_text())
        self.assertGreaterEqual(len(constants), 4, f"expected four queries, found {sorted(constants)}")
        for name, sql in constants.items():
            self.assertNotRegex(sql, r"\bFROM\s+fans\b", name)

    def test_durable_learning_and_channel_roi_share_one_retention_predicate(self) -> None:
        attributed = ATTRIBUTED.read_text()
        channel_yield = CHANNEL_YIELD.read_text()
        self.assertEqual(
            attributed.count("fan_is_meaningfully_retained("),
            2,
            "both attributed count query variants must use the canonical retention predicate",
        )
        self.assertIn("fan_is_meaningfully_retained(", channel_yield)


if __name__ == "__main__":
    unittest.main()
