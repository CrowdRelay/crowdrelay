#!/usr/bin/env python3
"""`degraded` has to say which phase.

`viryaos_autopilot_cycle_runs.outcome` is `succeeded` or `degraded`, and
`degraded` came from one boolean that eighteen call sites in the autopilot cycle
could set. The row recorded that a phase fell over and never which one.

Production 2026-09-13: 296 cycles in 24 hours, 40 degraded. Finding out what
those 40 hit meant grepping worker logs by timestamp, so the answer expired with
the logs. `/v1/admin/ops/cycles?state=degraded` could list them and explain none.

`degraded` rather than `failed` is deliberate — the phases are isolated, so one
failing while the rest complete is the design working. That is exactly why the
phase name matters: a 13% degraded rate is either isolation absorbing transient
errors or one phase broken every cycle, and those call for opposite responses.

Three things have to hold together or the column goes quietly wrong:

  * every failure site names a phase from the constant list, so a new phase
    cannot be recorded as an ad-hoc string that no reader recognises;
  * every declared constant is actually used, so the list does not accumulate
    names nothing can produce;
  * the read surface still returns the column, because a recorded value nobody
    can see is the situation this replaced.
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
WORKER = ROOT / "crates/crowdrelay-worker/src/autopilot.rs"
TRIGGER = ROOT / "crates/crowdrelay-infra/src/autopilot/cycle_trigger.rs"
LEDGER = ROOT / "crates/crowdrelay-api/src/ops_action_ledger.rs"
MIGRATIONS = ROOT / "migrations"


def phase_constants() -> dict[str, str]:
    source = WORKER.read_text()
    block = source[source.index("mod phase {") :]
    block = block[: block.index("\n}\n")]
    return {
        match.group("name"): match.group("value")
        for match in re.finditer(
            r'pub const (?P<name>[A-Z_]+): &str = "(?P<value>[a-z_]+)";', block
        )
    }


def failure_sites() -> list[str]:
    return re.findall(r"degraded\.failed\((?P<arg>[^)]*)\)", WORKER.read_text())


class CycleDegradedPhases(unittest.TestCase):
    def test_the_phase_list_is_not_empty(self):
        self.assertGreaterEqual(
            len(phase_constants()),
            10,
            "the phase constants moved or were renamed; the rest of this file "
            "is reasoning about nothing",
        )

    def test_every_failure_site_names_a_declared_phase(self):
        declared = {f"phase::{name}" for name in phase_constants()}
        sites = failure_sites()
        self.assertGreaterEqual(
            len(sites), 15, f"expected the cycle's failure sites, found {len(sites)}"
        )
        for argument in sites:
            self.assertIn(
                argument.strip(),
                declared,
                "a cycle failure must name a declared phase, not an ad-hoc "
                "string: a value no reader recognises is no better than the "
                "boolean this replaced",
            )

    def test_every_declared_phase_is_reachable(self):
        source = WORKER.read_text()
        for name, value in phase_constants().items():
            uses = len(re.findall(rf"phase::{name}\b", source))
            self.assertGreaterEqual(
                uses,
                1,
                f"phase {value} is declared and never recorded; a name nothing "
                "can produce makes the list lie about what the cycle does",
            )

    def test_the_phase_values_are_stable_identifiers(self):
        """They are read by an operator and stored in history, so they are a
        contract — not prose like the log messages they replaced."""
        for name, value in phase_constants().items():
            self.assertRegex(
                value,
                r"^[a-z][a-z0-9_]*$",
                f"{name} must be a stable snake_case identifier",
            )

    def test_the_outcome_is_derived_from_the_phase_list(self):
        """Two sources for one fact drift. `degraded` must mean "a phase is
        named", not a separate boolean that can disagree with the list."""
        trigger = TRIGGER.read_text()
        self.assertRegex(
            trigger,
            r"outcome = CASE WHEN cardinality\(\$4::text\[\]\) > 0 THEN 'degraded'",
            "the cycle outcome must be derived from the recorded phases",
        )

    def test_the_column_exists_and_distinguishes_empty_from_null(self):
        created = [
            path
            for path in sorted(MIGRATIONS.glob("*.sql"))
            if "degraded_phases" in path.read_text()
        ]
        self.assertTrue(created, "no migration adds degraded_phases")
        schema = created[0].read_text()
        self.assertIn("text[]", schema)
        # The distinction is the whole readability of the column as history.
        self.assertIn("NULL", schema)
        self.assertIn("empty", schema)

    def test_the_read_surface_returns_it(self):
        ledger = LEDGER.read_text()
        self.assertIn(
            "degraded_phases",
            ledger,
            "/v1/admin/ops/cycles must return the phases; a recorded value "
            "nobody can read is the situation this replaced",
        )
        query = ledger[ledger.index("FROM viryaos_autopilot_cycle_runs") - 800 :]
        query = query[: query.index("FROM viryaos_autopilot_cycle_runs")]
        self.assertIn(
            "degraded_phases",
            query,
            "the column must be selected, not only present in the response type",
        )


if __name__ == "__main__":
    unittest.main()
