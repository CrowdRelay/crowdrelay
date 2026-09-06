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

    def test_the_cycle_does_not_default_a_failed_repository_read(self) -> None:
        """A read that failed is not a reading of zero.

        Three loads in the growth-intelligence cycle swallowed their errors.
        Each had a legitimate `Ok` for the empty case, so the default could only
        ever fire on a failure, and each answered the failure with a specific
        false claim:

        - `count_pending_measurements` -> 0 erased WAIT's entire
          value-of-information, making a database error push an autonomous
          system towards acting.
        - `load_last_dispatched_template` -> None reported no previous
          strategy, switching off the hysteresis that exists to stop the brain
          flip-flopping on a borderline world model.
        - `load_exploration_memory` -> empty claimed every (template, context)
          pair unvisited, maximum novelty everywhere.

        None of them logged. The cycle reports degraded when a load propagates,
        which is the whole difference: a visible failure instead of a confident
        wrong number.
        """
        source = (
            ROOT
            / "crates/crowdrelay-application/src/autopilot/evaluate/growth_intelligence_context.rs"
        ).read_text()
        swallowed = re.findall(
            r"\.await\s*\n\s*\.(unwrap_or|unwrap_or_default|unwrap_or_else|ok\(\))[^;]*;",
            source,
        )
        # The strategy posterior load is the documented exception: it is
        # dormant, nothing reads it, and it is no longer written back — so its
        # default cannot reach a decision or overwrite learned state. If it ever
        # gains a consumer, the dormancy gate above fires first.
        self.assertLessEqual(
            len(swallowed),
            1,
            f"a repository read in the growth-intelligence cycle is defaulting "
            f"its error instead of propagating: {swallowed}",
        )

    def test_the_dormant_world_model_fields_are_still_dormant(self) -> None:
        """`WorldModel`'s dormant set must stay out of the decision path.

        `WorldModel` is never persisted and never returned from an endpoint, so
        a field nothing reads in this workspace is a field nothing reads
        anywhere. Eight are in that position and five of them cost a dedicated
        query per cycle. They are kept rather than deleted because the counters
        are correct — expensively so, after a LEFT JOIN fan-out made every post
        add a phantom community — and `pipeline_counts_count_places_not_posts`
        is the live-Postgres proof of that fix.

        Kept, but not misrepresented. The failure mode that test describes —
        posting more making the brain believe it needs fewer places — cannot
        happen while nothing consults the count. Wiring one up is the good
        outcome; it moves out of the doc's dormant list in the same change.
        """
        dormant = [
            "discovered_communities",
            "active_communities",
            "avg_community_engagement_bps",
            "best_performing_community",
            "worst_performing_community",
            "pending_outreach_targets",
            "promoted_outreach_targets",
            "engaged_outreach_targets",
        ]
        loader = (
            ROOT
            / "crates/crowdrelay-infra/src/autopilot/operations/growth_intelligence.rs"
        )
        model = ROOT / "crates/crowdrelay-brain/src/world_model.rs"
        for field in dormant:
            readers = [
                str(path.relative_to(ROOT))
                for path, text in production_sources()
                if path not in (loader, model)
                and re.search(r"\.\s*" + field + r"\b", text)
            ]
            self.assertEqual(
                readers,
                [],
                f"`WorldModel::{field}` is documented as dormant and {readers} "
                f"now reads it. If the brain genuinely uses it, move it out of "
                f"the dormant list in world_model.rs and out of this one",
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
