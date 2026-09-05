#!/usr/bin/env python3
"""`docs/BRAIN_LEARNING_LOOP.md` must keep describing this codebase.

The document is the one place the whole cycle is written down: what owns each
value, what `UNKNOWN` means at each edge, and — the part that decays fastest —
which values are computed and read by nobody. A map of the brain that has
drifted from the brain is worse than no map, because it is trusted.

So this gate holds the two things a reader would be misled by:

1. Every symbol the document names as load-bearing still exists.
2. Every edge it calls **dormant** is still dormant. A dormant value that
   quietly acquires a consumer is the good outcome — but it has to arrive with
   the document corrected in the same change, not six months later when someone
   is trying to work out why a decision came out the way it did.
"""
from __future__ import annotations

import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DOC = ROOT / "docs/BRAIN_LEARNING_LOOP.md"
RUST = sorted((ROOT / "crates").rglob("*.rs"))

# Symbols the document asserts exist, and the file that must define them.
NAMED_SYMBOLS = {
    "crates/crowdrelay-brain/src/decision_value.rs": [
        "pub fn total(",
        "pub risk_penalty",
        "pub calibration_bias",
        "pub contamination",
    ],
    "crates/crowdrelay-domain/src/action_ledger.rs": [
        "pub enum SuccessEvidence",
        "pub fn legal_transition(",
        "pub fn from_action_status(",
    ],
    "crates/crowdrelay-infra/src/autopilot/success_evidence.rs": [
        "pub(super) async fn locked_action_state(",
    ],
    "crates/crowdrelay-infra/src/autopilot/measurement/readiness.rs": [
        "pub(super) async fn refresh_evidence_readiness(",
        "pub(super) async fn measured_evidence_quality(",
    ],
    "crates/crowdrelay-infra/src/autopilot/operations/growth_intelligence/evidence_replay.rs": [
        "fn control_arm_means<",
        "async fn apply_evidence_to_stored_strategy_posterior(",
    ],
    "crates/crowdrelay-brain/src/causal_model.rs": [
        "correct_prediction_by_regime(",
    ],
}

# Values the document lists as dormant, as (label, regex that would prove a
# consumer). A match means the value is now read somewhere, and the table is
# out of date.
DORMANT = [
    (
        "DecisionValue::calibration_bias",
        re.compile(r"\.calibration_bias\s*[*+\-/]|[*+\-/]\s*\w+\.calibration_bias"),
    ),
    (
        "DecisionValue::contamination",
        re.compile(r"\.contamination\s*[*+\-/]|[*+\-/]\s*\w+\.contamination"),
    ),
    (
        "StateConditionedStrategyPosterior",
        re.compile(r"strategy_posterior\s*\.\s*(predict|confidence)\s*\("),
    ),
]


def production_sources() -> list[Path]:
    """Non-test Rust sources. Tests may construct dormant values freely."""
    out = []
    for path in RUST:
        if "/tests/" in str(path) or path.name.endswith("_tests.rs"):
            continue
        text = path.read_text()
        # Drop `#[cfg(test)] mod tests` tails so in-file unit tests don't
        # register as consumers.
        marker = text.find("#[cfg(test)]\nmod tests")
        if marker != -1:
            text = text[:marker]
        out.append((path, text))
    return out


class BrainLearningLoopDoc(unittest.TestCase):
    def test_the_document_exists_and_covers_the_cycle(self) -> None:
        doc = DOC.read_text()
        for stage in (
            "OBJECTIVE",
            "WORLD STATE",
            "BELIEF",
            "PREDICTION",
            "CANDIDATES",
            "ECONOMIC VALUE",
            "PORTFOLIO",
            "AUTHORITY",
            "EXECUTION",
            "MEASUREMENT",
            "BELIEF UPDATE",
        ):
            self.assertIn(
                stage, doc, f"the canonical loop no longer names the {stage} stage"
            )

    def test_every_named_symbol_still_exists(self) -> None:
        for relative, symbols in NAMED_SYMBOLS.items():
            source = (ROOT / relative).read_text()
            for symbol in symbols:
                self.assertIn(
                    symbol,
                    source,
                    f"docs/BRAIN_LEARNING_LOOP.md names `{symbol}` in {relative} "
                    f"and it is not there. The map has drifted from the code",
                )

    def test_the_dormant_edges_are_still_dormant(self) -> None:
        sources = production_sources()
        for label, pattern in DORMANT:
            consumers = [
                str(path.relative_to(ROOT))
                for path, text in sources
                if pattern.search(text)
            ]
            self.assertEqual(
                consumers,
                [],
                f"`{label}` is documented as dormant — written and read by no "
                f"decision — but {consumers} now consumes it. If that is "
                f"intentional, update the dormant table in "
                f"docs/BRAIN_LEARNING_LOOP.md and this list together",
            )


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    if result.wasSuccessful():
        print(f"BRAIN_LEARNING_LOOP=PASS dormant={len(DORMANT)}")
    else:
        print("BRAIN_LEARNING_LOOP=FAIL")
        sys.exit(1)
