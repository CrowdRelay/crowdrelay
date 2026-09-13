#!/usr/bin/env python3
"""The worker health fields must measure what their names say.

`crash_looping` read its cycle age from `MAX(evaluated_at)` on
`viryaos_autopilot_decisions` — the last *decision*, not the last *cycle*. A
cycle that runs correctly and finds nothing worth deciding writes no decision
row, so thirty minutes of healthy empty cycles raised a crash-loop alarm.

Measured in production 2026-09-13: `crash_looping: true` against a worker with
`RestartCount=0`, `ExitCode=0`, and eight consecutive `succeeded` cycles each
finishing in 300-1700ms. `ops/attention` is the operator's exception-first view
and this was its loudest field, pointing at nothing.

`viryaos_autopilot_cycle_runs` (migration 0233) records cycle completion, and
its own schema comment names the case the signal wants: "NULL means the cycle
never finished: the process died mid-cycle, which is otherwise
indistinguishable from a cycle that ran and decided nothing."
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SUMMARY = (ROOT / "crates/crowdrelay-api/src/ops_summary.rs").read_text()


def statement_for(binding: str) -> str:
    """The SQL literal feeding a binding, up to the fetch that runs it."""
    start = SUMMARY.index(binding)
    return SUMMARY[start : SUMMARY.index(".await?;", start)]


class WorkerHealthSignalContract(unittest.TestCase):
    def test_cycle_age_comes_from_finished_cycles(self):
        sql = statement_for("let (cycle_age_seconds, decision_age_seconds)")
        self.assertIn("MAX(finished_at)", sql)
        self.assertIn("FROM viryaos_autopilot_cycle_runs", sql)

    def test_decision_age_is_reported_separately_rather_than_conflated(self):
        # Keeping it is the point: "cycles run but decide nothing" is a real
        # growth-loop observation. It is just not a crash loop, and reporting it
        # as one sent an operator looking for a process fault that did not exist.
        sql = statement_for("let (cycle_age_seconds, decision_age_seconds)")
        self.assertIn("MAX(evaluated_at)", sql)
        self.assertIn("FROM viryaos_autopilot_decisions", sql)
        self.assertIn("pub(crate) decision_age_seconds: i64", SUMMARY)

    def test_crash_looping_is_derived_from_the_cycle_age_and_the_lease(self):
        self.assertIn(
            "let crash_looping = alive && cycle_age_seconds > WORKER_CYCLE_STALE_AFTER_SECONDS;",
            SUMMARY,
        )
        # A stale lease means dead, not looping; the `alive &&` is what separates
        # them and dropping it would report every dead worker as a crash loop.
        self.assertNotIn(
            "let crash_looping = cycle_age_seconds >", SUMMARY
        )

    def test_crash_looping_never_reads_the_decision_age(self):
        # The regression this file exists for. Both ages are in scope at the
        # point the boolean is computed, so the wrong one is one word away.
        derivation = SUMMARY[SUMMARY.index("let crash_looping") :]
        derivation = derivation[: derivation.index("\n")]
        self.assertNotIn("decision_age_seconds", derivation)

    def test_both_ages_default_to_never_rather_than_to_healthy(self):
        # COALESCE to 999999, not to 0: a worker that has never run a cycle must
        # read as stale, not as one that just finished.
        sql = statement_for("let (cycle_age_seconds, decision_age_seconds)")
        self.assertEqual(len(re.findall(r"\), 999999\)", sql)), 2)

    def test_the_stale_threshold_covers_several_normal_cycles(self):
        match = re.search(
            r"const WORKER_CYCLE_STALE_AFTER_SECONDS: i64 = (\d+);", SUMMARY
        )
        self.assertIsNotNone(match)
        threshold = int(match.group(1))
        # Production ticks the autopilot every 300s. A threshold under a few
        # cycles turns one slow cycle into a crash-loop alarm.
        self.assertGreaterEqual(threshold, 300 * 4)


if __name__ == "__main__":
    unittest.main()
