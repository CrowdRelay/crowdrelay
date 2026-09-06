#!/usr/bin/env python3
"""One number decides what "clean experiment" means, and it must stay one number.

`CONTAMINATION_CEILING` is the line between a randomised assignment that is
still a clean experiment and one whose unit received enough concurrent activity
that the randomisation no longer identifies anything. Its own doc comment claims
"one number, three call sites, so they cannot drift into disagreeing about what
clean means".

Two of those three were literals.

`evaluate_contamination` stamped `final_evidence_quality` with `> 0.1`;
`mark_causal_credits` decided whether a credit may be called *causal* with a
`0.1` inside a SQL string; `GrowthEvidence::effective_evidence_quality` used the
constant with `<`. At exactly the ceiling the three disagreed: the row was
stamped `randomized_holdout` and then refused causal status and capped to
quasi-experimental by everything that read it. The stored claim was the wrong
one, and the stored claim is what an operator reads.

So: no bare contamination threshold anywhere. Every site binds or references the
constant, and the accept condition is expressed one way — `< CEILING` is clean,
everything else is not.
"""
from __future__ import annotations

import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
EVIDENCE = ROOT / "crates/crowdrelay-brain/src/evidence.rs"

# Files that decide, in any form, whether an assignment is a clean experiment.
DECIDING_SITES = [
    "crates/crowdrelay-brain/src/evidence.rs",
    "crates/crowdrelay-infra/src/autopilot/operations/attribution.rs",
    "crates/crowdrelay-infra/src/autopilot/operations/experiment_assignments.rs",
]

# A contamination comparison against a bare number, in Rust or in SQL.
BARE_THRESHOLD = re.compile(
    r"(?:final_)?contamination\s*(?:<|>|<=|>=)\s*[0-9]", re.IGNORECASE
)


def production_source(relative: str) -> str:
    """The file without its `#[cfg(test)]` tail — fixtures may use literals."""
    text = (ROOT / relative).read_text()
    marker = text.find("#[cfg(test)]")
    return text[:marker] if marker != -1 else text


class ContaminationCeiling(unittest.TestCase):
    def test_the_ceiling_is_declared_once(self) -> None:
        declarations = re.findall(
            r"pub const CONTAMINATION_CEILING: f64 = ([0-9.]+);", EVIDENCE.read_text()
        )
        self.assertEqual(
            len(declarations),
            1,
            f"CONTAMINATION_CEILING must be declared exactly once, found "
            f"{declarations}",
        )

    def test_no_deciding_site_compares_against_a_bare_number(self) -> None:
        for relative in DECIDING_SITES:
            source = production_source(relative)
            offenders = BARE_THRESHOLD.findall(source)
            self.assertEqual(
                offenders,
                [],
                f"{relative} compares contamination against a literal "
                f"({offenders}). Bind or reference CONTAMINATION_CEILING — a "
                f"matching literal is not the same number, it is a number that "
                f"currently matches",
            )

    def test_every_deciding_site_reaches_the_constant(self) -> None:
        for relative in DECIDING_SITES:
            source = production_source(relative)
            self.assertIn(
                "CONTAMINATION_CEILING",
                source,
                f"{relative} decides what counts as a clean experiment without "
                f"naming the constant that defines it",
            )

    def test_clean_is_expressed_one_way(self) -> None:
        """`< CEILING` is clean. The inverse is not written as its own rule.

        `evaluate_contamination` expressed the same boundary as `> 0.1` and
        inverted at exactly the ceiling. Both sides now read as the negation of
        one accept condition, so there is no second rule to disagree with the
        first.
        """
        for relative in DECIDING_SITES:
            source = production_source(relative)
            for inverted in (
                "contamination > CONTAMINATION_CEILING",
                "contamination >= CONTAMINATION_CEILING",
                "final_contamination > CONTAMINATION_CEILING",
                "final_contamination >= CONTAMINATION_CEILING",
            ):
                self.assertNotIn(
                    inverted,
                    source,
                    f"{relative} states the contamination boundary in the "
                    f"reject direction. Write the accept condition "
                    f"(`< CONTAMINATION_CEILING`) and take the else branch, so "
                    f"the two sides cannot disagree about the endpoint",
                )


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    if result.wasSuccessful():
        print(f"CONTAMINATION_CEILING=PASS sites={len(DECIDING_SITES)}")
    else:
        print("CONTAMINATION_CEILING=FAIL")
        sys.exit(1)
