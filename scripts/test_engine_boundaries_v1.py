"""Contract tests for the engine/domain boundary.

The decision engine — authority ladder, class ceilings, envelope,
deliverability, learning — is generic machinery. Music lives in bounded
contexts (`plays`, `beacons`, `growth_metrics`, ...) and infra adapters.
Repurposing the engine for another vertical means writing new contexts and
adapters; it must never mean editing engine core.

These tests pin that boundary in code, so it cannot erode one import at a
time.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
DOMAIN = ROOT / "crates/crowdrelay-domain/src"

# Engine-core modules: domain-agnostic mechanics. Each must not import or name
# any bounded-context module.
CORE = [
    "autonomy",
    "action_class",
    "growth_envelope",
    "deliverability",
    "learning",
    "performance",
    "next_best_action",
]

# Bounded-context modules: music-specific (and where any future vertical's
# contexts would sit). Engine core must not reference these.
CONTEXTS = [
    "acquisition", "audience_lifecycle", "beacon_release", "beacons",
    "booking", "campaign_lifecycle", "content_supply", "events",
    "experimentation", "fan_activation", "fan_lifecycle", "fan_privacy",
    "free_reach", "funding", "growth_debt", "growth_metrics", "live_opportunities",
    "market_intelligence", "merch_bundle", "merchandising", "negotiation",
    "objectives", "outreach", "playlist_placement",
    "play_measurement", "plays", "pricing", "promotion", "release_autopilot",
    "show_operations", "show_settlement", "show_growth", "target_discovery",
    "tour_economics", "mobile_fan",
]

# Prose words allowed inside comments/doc examples; identifiers are not.
IDENTIFIER = re.compile(r"\b(?:%s)\b" % "|".join(CONTEXTS))


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def strip_comments(source: str) -> str:
    """Drop `//` line comments and `///` doc comments; keep code only."""
    out = []
    for line in source.splitlines():
        stripped = re.sub(r"^\s*//.*$", "", line)
        out.append(stripped)
    return "\n".join(out)


class EngineBoundaryContract(unittest.TestCase):
    def test_core_modules_exist(self) -> None:
        for name in CORE:
            self.assertTrue((DOMAIN / f"{name}.rs").exists(), name)

    def test_engine_core_never_imports_a_bounded_context(self) -> None:
        # The whole point: a new vertical swaps contexts and adapters; the
        # mechanics must not know they exist, even by importing one of their
        # types. Only import paths are checked — prose examples in comments
        # are allowed to speak English.
        for name in CORE:
            source = strip_comments(read(DOMAIN / f"{name}.rs"))
            for line in source.splitlines():
                if not line.strip().startswith("use "):
                    continue
                match = IDENTIFIER.search(line)
                if match:
                    self.fail(
                        f"engine-core module `{name}` imports bounded-context "
                        f"module `{match.group(0)}`: {line.strip()}"
                    )

    def test_the_learning_rule_takes_facts_not_policies(self) -> None:
        # `effective_recipient_ceiling` used to take a PlayPolicy — the one
        # place engine core reached into a context's types. It now takes the
        # plain number, which is what made this boundary enforceable.
        learning = read(DOMAIN / "learning.rs")
        self.assertIn(
            "pub fn effective_recipient_ceiling(max_recipients_per_step: u32",
            learning,
        )
        self.assertNotIn("plays::PlayPolicy", learning)

    def test_context_modules_are_declared_as_such_in_the_crate_doc(self) -> None:
        lib = read(DOMAIN / "lib.rs")
        self.assertIn("Engine core", lib)
        self.assertIn("Bounded contexts", lib)
        self.assertIn("test_engine_boundaries_v1.py", lib)

    def test_the_posture_mapping_is_the_one_documented_exception(self) -> None:
        posture = read(
            ROOT / "crates/crowdrelay-application/src/autopilot/growth_posture.rs"
        )
        self.assertIn("AutopilotContext", posture)
        # Its doc says why: the template sits beside the context list on
        # purpose, holding no I/O of its own.
        self.assertIn("I/O of any kind", posture)

    def test_generic_mechanisms_take_parameters_not_domains(self) -> None:
        # Spot-checks on the pattern that keeps core generic: thresholds and
        # ceilings arrive as arguments, never as imports of a context type.
        envelope = strip_comments(read(DOMAIN / "growth_envelope.rs"))
        self.assertNotIn("PlayPolicy", envelope)
        deliverability = strip_comments(read(DOMAIN / "deliverability.rs"))
        self.assertNotIn("OutreachPolicy", deliverability)


if __name__ == "__main__":
    unittest.main()
