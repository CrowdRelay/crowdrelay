#!/usr/bin/env python3
"""Two definitions of the goal, and only one of them decides anything.

`domain::objectives::GrowthObjective` is an operator-declared target on a
measured series: platform, metric key, scope, direction, a frozen baseline, a
target and a deadline. `assess_objective` judges it and refuses to guess —
no observation, or under 72 hours elapsed, is `Unmeasurable { reason }`, not
"on track". It reaches the API and the chief briefing.

`brain::world_model::GrowthTarget` is derived from a hardcoded fan-count table
with no deadline and no operator input, and it is the one the brain optimises
toward. So an operator can declare "100 durable fans by the 27th", watch it turn
`Behind`, and the brain will not have changed a single decision — the number it
is working toward is `max(north_star_current / 10, 5)`.

That gap is documented in `docs/GOAL_DIRECTED_CONTROL.md`, which describes the
contract for closing it. This gate holds the document to the code:

1. The document exists and still names the four things it forbids.
2. The claim it is built on — that no evaluation path reads an objective — is
   still true. Closing the gap is the intended outcome; it has to arrive with
   the "What is missing" section updated in the same change, rather than
   leaving a document that describes a gap someone already closed.
"""
from __future__ import annotations

import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DOC = ROOT / "docs/GOAL_DIRECTED_CONTROL.md"
EVALUATE = ROOT / "crates/crowdrelay-application/src/autopilot/evaluate"
BRAIN = ROOT / "crates/crowdrelay-brain/src"

# `Objective` as a domain type, not the word "objective" in prose — the
# portfolio's doc comments discuss submodular objective functions, which is a
# different sense entirely.
OBJECTIVE_TYPE = re.compile(
    r"\b(GrowthObjective|ObjectiveState|ObjectiveScope|ObjectiveGap"
    r"|assess_objective|load_objectives|objectives::)"
)


def rust_sources(root: Path) -> list[tuple[Path, str]]:
    out = []
    for path in sorted(root.rglob("*.rs")):
        if path.name.endswith("_tests.rs"):
            continue
        text = path.read_text()
        marker = text.find("#[cfg(test)]\nmod tests")
        if marker != -1:
            text = text[:marker]
        out.append((path, text))
    return out


class GoalDirectedControlContract(unittest.TestCase):
    def test_the_contract_document_exists_and_states_its_limits(self) -> None:
        doc = DOC.read_text()
        for forbidden in (
            "A second planner",
            "A second definition of progress",
            "A trajectory model",
            "Deadline-driven safety overrides",
        ):
            self.assertIn(
                forbidden,
                doc,
                f"docs/GOAL_DIRECTED_CONTROL.md no longer forbids `{forbidden}`. "
                f"The limits are the load-bearing half of the contract — without "
                f"them it is a feature request",
            )
        self.assertIn(
            "must **not** enter `DecisionValue::total()`",
            doc,
            "the contract no longer says where a deadline may act. Urgency is "
            "not value: a deadline does not make a bad action better, and a "
            "pace term in total() is the weighted soup the invariant forbids",
        )

    def test_no_evaluation_path_reads_an_objective_yet(self) -> None:
        readers = [
            str(path.relative_to(ROOT))
            for path, text in rust_sources(EVALUATE) + rust_sources(BRAIN)
            if OBJECTIVE_TYPE.search(text)
        ]
        self.assertEqual(
            readers,
            [],
            f"{readers} now reads an operator objective on a decision path. "
            f"That is the intended destination — update the 'What is missing' "
            f"section of docs/GOAL_DIRECTED_CONTROL.md and this gate together, "
            f"and check the change lands in PortfolioConfig or DecisionMode "
            f"rather than in DecisionValue::total()",
        )

    def test_the_derived_target_is_still_the_one_the_brain_uses(self) -> None:
        """The document's table is only true while this is."""
        model = (BRAIN / "world_model.rs").read_text()
        self.assertIn(
            "pub fn from_fan_count(",
            model,
            "GrowthTarget::from_fan_count is gone; the document describes it as "
            "the target the brain optimises toward and must be corrected",
        )


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    if result.wasSuccessful():
        print("GOAL_DIRECTED_CONTROL=PASS")
    else:
        print("GOAL_DIRECTED_CONTROL=FAIL")
        sys.exit(1)
