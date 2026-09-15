#!/usr/bin/env python3
"""The plan's claims about this codebase must be true of this codebase.

`CROWDRELAY_LEVERAGE_PLAN.md` decides what gets built. On 2026-09-15 its §4c
listed eight brain modules under the heading "already answers these questions".
`change_point` runs every cycle and only writes a log line; `calibration`
records every prediction against its outcome and nothing reads it; the
hypothesis promotion ladder is called by nothing at all. §2 of the same document
is titled "do not rebuild", so a module listed there is a module nobody builds,
and three states that are not "available" had been recorded as capability in
hand.

The same day, a CV was nearly published claiming that growth templates earn
dispatch budget through a promotion ladder. `hypothesis.rs` says in its own
module doc that the ladder "is a design nothing calls". Prose next to the code
caught what prose far from the code had got wrong.

This is the same failure `test_watchdog_conditions_documented_v1.py` exists for,
one level out: there, a module doc drifted from the function beneath it; here, a
plan drifts from the workspace. That gate's own words apply unchanged — this
repository "has a recorded habit of concluding a live capability is missing by
reading a stale list".

Three checks. A fourth was written and removed, and the reason is the point of
the gate.

- Every file path the plan names exists.
- Every module its wiring table names exists as a file.
- The counts the plan states — crates, migrations — are the real ones.

**Not checked: whether a module is wired.** The first version of this gate
asked whether a type name appears under `crates/*/src` outside the brain. That
is how the wrong answer was produced in the first place: `calibration` is
reached through the causal model's `record_by_regime`, which `evidence_replay`
drives, and `platform_yield` runs inside `rank_templates` — neither shows up
under its own type name. Four modules were called dead that are not. The
table's real statuses — "records, never read", "runs, unconsumed", "yes,
self-gating" — come from reading call graphs, and a grep cannot produce them.
Encoding that method here would have frozen the error into CI, which is worse
than having no gate. Wiring stays a human read.

The plan is the input, so it must be readable from the repository. Set
CROWDRELAY_PLAN to point at it; the gate skips with a clear message when the
plan is absent, because CI for a code change should not fail on a document that
was never checked in.
"""

import os
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

CANDIDATES = [
    Path(os.environ["CROWDRELAY_PLAN"]) if os.environ.get("CROWDRELAY_PLAN") else None,
    ROOT / "docs/plans/CROWDRELAY_LEVERAGE_PLAN.md",
    ROOT.parent / "CROWDRELAY_LEVERAGE_PLAN.md",
]

# The wiring table lives in one section, and the plan has many other tables —
# a content catalogue with a `name` column matched the first version of this.
# Find the section by its header, then read rows only from it.
WIRING_SECTION = re.compile(r"^## 4c\..*?(?=^## )", re.M | re.S)
# | `module.rs` | yes — 9 files | use |
TABLE_ROW = re.compile(r"^\|\s*`([a-z_]+)(?:\.rs)?`\s*\|\s*([^|]+?)\s*\|", re.M)
# A path the plan names. It writes them crate-relative — `crowdrelay-brain/src/
# portfolio.rs` — not from the workspace root, so the `crates/` prefix is added
# here. The first version required the prefix and matched nothing at all, which
# is the quietest way for a check to be useless.
PATH_REF = re.compile(r"`((?:crates/)?crowdrelay-[a-z]+/src/[A-Za-z0-9_/]+\.rs)`")
# The plan's one hard measurement of the brain: "22.7k lines and 460 tests".
# Earlier versions of this gate checked for "N migrations" and "eight crates",
# phrasings the plan never uses, so two of three checks passed by matching
# nothing. A check that cannot fail is worse than no check, because it reports
# success. Each pattern below is asserted to match before it is compared.
BRAIN_LINES = re.compile(r"`?crowdrelay-brain`?[^.]*?([\d.]+)k lines")
BRAIN_TESTS = re.compile(r"([\d,]+) tests")


def plan_path() -> Path | None:
    for candidate in CANDIDATES:
        if candidate and candidate.is_file():
            return candidate
    return None


class PlanClaims(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        path = plan_path()
        if path is None:
            raise unittest.SkipTest(
                "plan not found; set CROWDRELAY_PLAN or add it under docs/plans/"
            )
        cls.plan = path.read_text(encoding="utf-8")
        cls.path = path

    def test_every_path_the_plan_names_exists(self) -> None:
        refs = PATH_REF.findall(self.plan)
        self.assertGreater(
            len(refs), 0, "no file paths found in the plan — the pattern is wrong"
        )
        missing = sorted(
            {
                ref
                for ref in refs
                if not (ROOT / ref).exists()
                and not (ROOT / "crates" / ref).exists()
            }
        )
        self.assertEqual(
            missing,
            [],
            f"{self.path.name} names files that do not exist: {missing}",
        )

    def test_every_module_the_table_names_exists(self) -> None:
        """A table row for a module that was deleted or renamed.

        This is the checkable half of the wiring question. Whether production
        reaches a module is a judgement; whether the module is still there is a
        fact, and a plan that discusses a file nobody can open has drifted
        further than a plan that mis-describes one.
        """
        section = WIRING_SECTION.search(self.plan)
        if not section:
            self.skipTest("no §4c wiring section in this plan")
        missing = []
        for module, _claim in TABLE_ROW.findall(section.group(0)):
            if not list((ROOT / "crates").glob(f"*/src/{module}.rs")) and not list(
                (ROOT / "crates").glob(f"*/src/{module}/mod.rs")
            ):
                missing.append(module)
        self.assertEqual(
            missing, [], f"wiring table names modules that do not exist: {missing}"
        )

    def test_the_brain_measurements_are_real(self) -> None:
        """Numbers in prose are true on the day they are written and after.

        A tenth of a crate's size is noise; a third of it is a different claim.
        The tolerance is 15%, which catches a number that was copied forward
        through a year of growth without catching honest rounding.
        """
        brain = ROOT / "crates/crowdrelay-brain"
        real_lines = sum(
            len(f.read_text(encoding="utf-8", errors="ignore").splitlines())
            for f in brain.rglob("*.rs")
        )
        real_tests = sum(
            f.read_text(encoding="utf-8", errors="ignore").count("#[test]")
            for f in brain.rglob("*.rs")
        )

        stated_lines = BRAIN_LINES.search(self.plan)
        self.assertIsNotNone(stated_lines, "plan states no brain size — pattern stale")
        claimed = float(stated_lines.group(1)) * 1000
        self.assertLess(
            abs(claimed - real_lines) / real_lines,
            0.15,
            f"plan says {claimed:.0f} brain lines; there are {real_lines}",
        )

        stated_tests = BRAIN_TESTS.search(self.plan)
        self.assertIsNotNone(stated_tests, "plan states no test count — pattern stale")
        claimed_tests = int(stated_tests.group(1).replace(",", ""))
        self.assertLess(
            abs(claimed_tests - real_tests) / real_tests,
            0.15,
            f"plan says {claimed_tests} brain tests; there are {real_tests}",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
