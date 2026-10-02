#!/usr/bin/env python3
"""The operator's goal, and the bounded places it may change control.

`domain::objectives::GrowthObjective` is an operator-declared target on a
measured series: platform, metric key, scope, direction, a frozen baseline, a
target and a deadline. `assess_objective` judges it and refuses to guess —
no observation, or under 72 hours elapsed, is `Unmeasurable { reason }`, not
"on track".

It used to reach the API and the chief briefing and no decision: the brain
worked toward `GrowthTarget::from_fan_count`, a hardcoded monthly bucket, and an
objective turning `Behind` changed nothing. It is now wired, and
`docs/GOAL_DIRECTED_CONTROL.md` is the contract. This gate holds the document
and the code to it:

1. The document exists and still names the four things it forbids.
2. The objective reaches the brain through `goal.rs` alone, on
   `WorldModel.objective`.
3. It acts on the dispatch ceiling, exploration posture, and the narrow
   verified-organic read-only supply-recovery path, and nowhere else on the
   evaluation path.
4. It never enters `DecisionValue`, the optimizer's marginal, or anything about
   authority and approval.
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

    def test_the_objective_reaches_the_brain_through_the_goal_module(self) -> None:
        """Wired. The objective is read in the brain by `goal.rs` and carried on
        `WorldModel.objective`, and nowhere else on the brain side."""
        readers = sorted(
            str(path.relative_to(ROOT))
            for path, text in rust_sources(BRAIN)
            if OBJECTIVE_TYPE.search(text)
        )
        self.assertEqual(
            readers,
            ["crates/crowdrelay-brain/src/goal.rs"],
            f"{readers} read an operator objective in the brain. The goal "
            f"module is the one place an objective becomes a pace; a second "
            f"reader is a second definition of progress",
        )
        model = (BRAIN / "world_model.rs").read_text()
        self.assertIn(
            "pub objective: Option<crate::goal::ActiveObjective>",
            model,
            "WorldModel no longer carries the declared objective; the brain "
            "cannot see what the operator asked for",
        )

    def test_the_goal_acts_only_in_the_declared_control_paths(self) -> None:
        """The places the contract allows, and only those."""
        consumers = sorted(
            str(path.relative_to(ROOT))
            for path, text in rust_sources(EVALUATE)
            if re.search(r"\bobjective\b|ActiveObjective|GoalConstraint|GoalPace", text)
        )
        self.assertEqual(
            consumers,
            [
                "crates/crowdrelay-application/src/autopilot/evaluate/growth_intelligence.rs",
                "crates/crowdrelay-application/src/autopilot/evaluate/growth_intelligence/scout_consult.rs",
                "crates/crowdrelay-application/src/autopilot/evaluate/growth_intelligence_context.rs",
                "crates/crowdrelay-application/src/autopilot/evaluate/portfolio.rs",
            ],
            f"{consumers} read the goal on a decision path. It may act on "
            f"PortfolioConfig (portfolio.rs, called from the context arm), "
            f"on the exploration boost (growth_intelligence.rs), as declared "
            f"context in the scout's research brief, and in the context arm's "
            f"verified-organic supply-recovery gate. No other evaluator file "
            f"may consume it.",
        )
        portfolio = (EVALUATE / "portfolio.rs").read_text()
        self.assertIn("goal.applied_max_dispatches", portfolio)
        scoring = (EVALUATE / "growth_intelligence.rs").read_text()
        self.assertIn("ActiveObjective::is_behind", scoring)
        context = (EVALUATE / "growth_intelligence_context.rs").read_text()
        self.assertIn("organic_supply_recovery", context)
        self.assertIn('"verified_organic_acquisitions"', context)
        # The replenishment call stays in the context arm; the scout-candidate
        # builder it gates lives in the extracted supply_recovery submodule —
        # same contract, new home.
        self.assertIn("maybe_replenish_acquisition_supply", context)
        supply_recovery = (
            EVALUATE / "growth_intelligence" / "supply_recovery.rs"
        ).read_text()
        self.assertIn("supply_recovery_scout_candidate", supply_recovery)

    def test_urgency_never_enters_the_value(self) -> None:
        """A deadline does not make a bad action better."""
        for name in ("decision_value.rs", "portfolio.rs"):
            text = (BRAIN / name).read_text()
            self.assertIsNone(
                re.search(r"ActiveObjective|GoalConstraint|GoalPace|goal::", text),
                f"crates/crowdrelay-brain/src/{name} now reads the goal. The pace "
                f"must not enter DecisionValue::total() or the optimizer's "
                f"marginal — it is a ceiling on how many, never a term in how much",
            )
        goal = (BRAIN / "goal.rs").read_text()
        marker = goal.find("#[cfg(test)]")
        body = goal[:marker] if marker != -1 else goal
        code = "\n".join(
            line for line in body.splitlines() if not line.lstrip().startswith("//")
        )
        for forbidden in ("DecisionValue", "min_marginal_value", "authority", "approval"):
            self.assertNotIn(
                forbidden,
                code,
                f"goal.rs now touches `{forbidden}`. A goal may raise a budget "
                f"inside the envelope; it may never change value or widen authority",
            )

    def test_the_derived_target_survives_as_a_readout(self) -> None:
        """The monthly bucket stays for the operator; the declared objective is
        the goal when one is live."""
        model = (BRAIN / "world_model.rs").read_text()
        self.assertIn(
            "pub fn from_fan_count(",
            model,
            "GrowthTarget::from_fan_count is gone; the document describes it as "
            "the operator readout kept beside the declared objective and must be "
            "corrected",
        )


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    if result.wasSuccessful():
        print("GOAL_DIRECTED_CONTROL=PASS")
    else:
        print("GOAL_DIRECTED_CONTROL=FAIL")
        sys.exit(1)
