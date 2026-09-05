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
        "crates/crowdrelay-infra/src/autopilot/operations/growth_intelligence.rs"
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

    def test_the_dormant_posterior_is_labelled_dormant(self) -> None:
        """A learner nothing reads must say so where a reader will see it.

        `StateConditionedStrategyPosterior` is written every cycle and read by
        no decision. That is defensible — a belief needs history before it can
        be trusted — but its plumbing (threaded through the whole candidate
        pipeline) suggests otherwise, and the comments used to assert an
        influence on eligibility and exploration allocation that no code
        performed. If it is genuinely wired up later, this assertion is the
        reminder to correct the docs in the same change.
        """
        module = (
            ROOT / "crates/crowdrelay-brain/src/strategy_learning.rs"
        ).read_text()
        self.assertIn(
            "Dormant, not consumed",
            module,
            "the strategy posterior's dormancy is no longer documented; either "
            "it is now consumed (say where) or the label was dropped",
        )
        consumers = [
            p
            for p in RUST
            if "crowdrelay-brain" not in str(p)
            and re.search(r"strategy_posterior\s*\.\s*(predict|confidence)\s*\(", p.read_text())
        ]
        self.assertEqual(
            consumers,
            [],
            "the strategy posterior is being read on a decision path. That is "
            "the intended destination — update the dormancy docs in "
            "strategy_learning.rs and this gate together",
        )


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    if result.wasSuccessful():
        print(f"BRAIN_BELIEF_OWNERSHIP=PASS keys={len(SINGLE_WRITER_KEYS)}")
    else:
        print("BRAIN_BELIEF_OWNERSHIP=FAIL")
        sys.exit(1)
