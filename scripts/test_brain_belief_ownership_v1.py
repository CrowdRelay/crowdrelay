#!/usr/bin/env python3
"""Each brain belief has one writer, and one answer to its question.

Two failures this pins, both of which were live.

**Two writers, one key.** The `strategy_posterior` brain-state row was written
from two places in the same cycle. The infra loader folded resolved evidence
into it during the causal model load; the application's evaluation then loaded
that row, held it by shared reference, never mutated it, and wrote it back at
the end of the cycle. Harmless while the load succeeded — and the load ended in
`unwrap_or_default()`, so a deserialization failure or a read error produced an
empty posterior which was then saved over the learner's entire history. One
clobber, everything gone, no error raised anywhere. A belief with two writers
has no owner.

**Three learners, one question.** `crowdrelay-brain` exported `StrategyLearner`
(running averages), `StrategyPosterior` (unconditioned Normal-Normal with UCB)
and `StateConditionedStrategyPosterior`, all answering "which growth strategy
works". Only the third was reachable from production; the other two were
referenced by nothing outside their own tests, while sitting in the crate root's
public surface where a reader has to work out which one the brain runs on.

This gate reads source, not behaviour. It is deliberately cheap and blunt: it
catches the shape of the regression — a second `save_brain_state` call naming
the same key, or the duplicate types coming back — early enough to argue about.
"""
from __future__ import annotations

import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RUST = sorted((ROOT / "crates").rglob("*.rs"))

# Brain-state keys whose writer must be unique. The value is the file allowed
# to write it — the owner.
SINGLE_WRITER_KEYS = {
    "strategy_posterior": (
        "crates/crowdrelay-infra/src/autopilot/operations/growth_intelligence/evidence_replay.rs"
    ),
}


def save_sites(key: str) -> set[str]:
    """Files containing a `save_brain_state(...)` call naming `key`."""
    pattern = re.compile(
        r"save_brain_state\s*\([^;]*?\"" + re.escape(key) + r"\"",
        re.DOTALL,
    )
    hits: set[str] = set()
    for path in RUST:
        if path.name.endswith("_tests.rs") or "/tests/" in str(path):
            continue
        if pattern.search(path.read_text()):
            hits.add(str(path.relative_to(ROOT)))
    return hits


class BrainBeliefOwnership(unittest.TestCase):
    def test_each_brain_state_key_has_exactly_one_writer(self) -> None:
        for key, owner in SINGLE_WRITER_KEYS.items():
            writers = save_sites(key)
            self.assertEqual(
                writers,
                {owner},
                f"the `{key}` brain-state key must be written only by its owner. "
                f"A second writer holding a stale or defaulted copy overwrites "
                f"what the owner learned, and neither side can tell",
            )

    def test_one_strategy_belief_is_defined(self) -> None:
        """The duplicates must not come back under their old names."""
        module = (
            ROOT / "crates/crowdrelay-brain/src/strategy_learning.rs"
        ).read_text()
        for gone in ("pub struct StrategyLearner", "pub struct StrategyPosterior "):
            self.assertNotIn(
                gone,
                module,
                "a second strategy belief is back alongside "
                "StateConditionedStrategyPosterior. Three learners with three "
                "answers, two unreachable, is not redundancy",
            )

    def test_the_dormant_posterior_is_not_plumbed_through_the_decision_path(
        self,
    ) -> None:
        """The strategy posterior is now consumed by strategy selection.

        It was previously loaded, threaded through the candidate pipeline,
        and discarded. The posterior is now loaded and consumed by
        `GrowthStrategy::from_world_model_with_posterior`, which refines
        the rule-based strategy with learned evidence. This test pins that
        the load and consumption are both present, so removing either is a
        deliberate change to these signatures.
        """
        cycle = (
            ROOT
            / "crates/crowdrelay-application/src/autopilot/evaluate/growth_intelligence_context.rs"
        ).read_text()
        self.assertIn(
            'load_brain_state(self.workspace_id, "strategy_posterior")',
            cycle,
            "the growth-intelligence cycle must load the strategy posterior "
            "to refine the rule-based strategy with learned evidence",
        )
        self.assertIn(
            "from_world_model_with_posterior",
            cycle,
            "the strategy posterior must be consumed by "
            "GrowthStrategy::from_world_model_with_posterior",
        )

    def test_the_dormant_posterior_is_labelled_dormant(self) -> None:
        """The strategy posterior is now consumed — docs must say so.

        `StateConditionedStrategyPosterior` was previously written every
        cycle and read by no decision. It is now consumed by
        `GrowthStrategy::from_world_model_with_posterior`. The module docs
        must document the consumption so a reader knows the posterior is
        live, not dormant.
        """
        module = (
            ROOT / "crates/crowdrelay-brain/src/strategy_learning.rs"
        ).read_text()
        self.assertIn(
            "Consumed by strategy selection",
            module,
            "the strategy posterior is now consumed — the module docs "
            "must say 'Consumed by strategy selection' so a reader knows "
            "it is live, not dormant",
        )


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    if result.wasSuccessful():
        print(f"BRAIN_BELIEF_OWNERSHIP=PASS keys={len(SINGLE_WRITER_KEYS)}")
    else:
        print("BRAIN_BELIEF_OWNERSHIP=FAIL")
        sys.exit(1)
