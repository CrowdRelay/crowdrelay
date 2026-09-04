#!/usr/bin/env python3
"""A decision may never be recorded without saying what caused it.

`viryaos_autopilot_decisions` is the audit ledger. `trace_id` is the only thing
that joins a decision to the event that produced it, the action it emitted, the
attempts that action made and the outcome that came back -- the timeline an
operator reads to answer "why did the system do this?".

The column was nullable and the non-deterministic half of the system wrote NULL
into it. Measured in production: all 42 decisions with
`subject_kind = 'agent_outcome'` were untraced, because all 67 rows in
`agent_outcomes` carry a NULL `trace_id` and the mapper bound it straight
through. The deterministic paths were traced correctly, so the gap was invisible
in aggregate -- half the recent decisions had a trace and nobody asked which
half.

Migration 0231 backfilled the column and made it NOT NULL, which stops a NULL
reaching the table. This stops a writer being *written* that would try: an
`Option` bound into the trace column compiles, passes clippy, passes every test
that does not happen to exercise the `None` branch, and then fails on the first
real insert -- the standing risk of runtime-checked SQL that CLAUDE.md names.

Checked against source, so it runs on a CI runner with no database.
"""
from __future__ import annotations

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CRATES = ROOT / "crates"
MIGRATIONS = ROOT / "migrations"

DECISION_TABLE = "viryaos_autopilot_decisions"

# The three writers, all audited. A new one is not forbidden -- it just has to
# be added here deliberately, which is the point: the ledger's writers are few
# enough to name, and that is worth keeping true.
KNOWN_WRITERS = {
    "crates/crowdrelay-infra/src/autopilot/decisions/persist.rs",
    "crates/crowdrelay-infra/src/autopilot/team.rs",
    "crates/crowdrelay-worker/src/agent_outcomes.rs",
}


def rust_sources(include_tests: bool) -> list[Path]:
    paths: list[Path] = []
    for crate in sorted(CRATES.iterdir()):
        if not crate.is_dir():
            continue
        roots = [crate / "src"] + ([crate / "tests"] if include_tests else [])
        for root in roots:
            if root.is_dir():
                paths.extend(sorted(root.rglob("*.rs")))
    return paths


def insert_column_lists(text: str) -> list[str]:
    """The column list of every INSERT into the decision table in `text`."""
    pattern = rf"INSERT INTO {DECISION_TABLE}\s*\((.*?)\)\s*VALUES"
    return [match.group(1) for match in re.finditer(pattern, text, re.S)]


class EveryWriterSuppliesATrace(unittest.TestCase):
    def test_every_insert_names_the_trace_column(self) -> None:
        """Including tests: a fixture that skips it is a writer that skips it."""
        missing = []
        for path in rust_sources(include_tests=True):
            text = path.read_text(encoding="utf-8", errors="replace")
            if DECISION_TABLE not in text:
                continue
            for columns in insert_column_lists(text):
                if "trace_id" not in columns:
                    missing.append(str(path.relative_to(ROOT)))
        self.assertEqual(
            sorted(set(missing)),
            [],
            "these INSERT statements omit trace_id, so the decision they write "
            "cannot be joined to whatever caused it",
        )

    def test_no_production_writer_binds_an_optional_trace(self) -> None:
        """`.bind(outcome.trace_id)` on an `Option` is how the hole was opened.

        The migration now rejects the NULL at the database, which turns a silent
        orphan into a failed insert -- better, but still a production failure.
        The trace has to be resolved to a value before it is bound.
        """
        offenders = []
        for path in rust_sources(include_tests=False):
            text = path.read_text(encoding="utf-8", errors="replace")
            if f"INSERT INTO {DECISION_TABLE}" not in text:
                continue
            for line in text.splitlines():
                stripped = line.strip()
                if not stripped.startswith(".bind("):
                    continue
                # A bind naming a trace that is plainly an Option field read
                # straight off an inbound record.
                if re.search(r"\.bind\(\s*\w+\.trace_id\s*\)", stripped):
                    offenders.append(f"{path.relative_to(ROOT)}: {stripped}")
        self.assertEqual(
            offenders,
            [],
            "a trace is being bound straight from a record field; resolve it to "
            "a concrete value first so it can never be NULL",
        )

    def test_the_writer_set_is_still_the_audited_one(self) -> None:
        writers = set()
        for path in rust_sources(include_tests=False):
            text = path.read_text(encoding="utf-8", errors="replace")
            if f"INSERT INTO {DECISION_TABLE}" in text:
                writers.add(str(path.relative_to(ROOT)))
        self.assertEqual(
            writers,
            KNOWN_WRITERS,
            "the set of code that can write to the decision ledger changed. Audit "
            "the new writer for trace correlation, then update KNOWN_WRITERS",
        )


class TheColumnIsEnforced(unittest.TestCase):
    def test_a_migration_makes_the_column_not_null(self) -> None:
        found = any(
            re.search(
                rf"ALTER TABLE\s+{DECISION_TABLE}\s+ALTER COLUMN trace_id SET NOT NULL",
                path.read_text(encoding="utf-8", errors="replace"),
                re.S,
            )
            for path in MIGRATIONS.glob("*.sql")
        )
        self.assertTrue(
            found,
            "no migration makes viryaos_autopilot_decisions.trace_id NOT NULL; "
            "without it the column is an invitation for the next writer to skip",
        )

    def test_no_later_migration_relaxes_it(self) -> None:
        """`DROP NOT NULL` here would silently reopen the hole."""
        relaxed = [
            path.name
            for path in sorted(MIGRATIONS.glob("*.sql"))
            if re.search(
                rf"ALTER TABLE\s+{DECISION_TABLE}\s+ALTER COLUMN trace_id DROP NOT NULL",
                path.read_text(encoding="utf-8", errors="replace"),
                re.S,
            )
        ]
        self.assertEqual(relaxed, [], f"a migration drops the NOT NULL: {relaxed}")

    def test_the_column_has_no_default(self) -> None:
        """A default would hand a forgetful writer a meaningless random root.

        That is worse than the NULL it replaces: a NULL is visibly missing, a
        random UUID looks like a real correlation and joins to nothing.
        """
        defaulted = [
            path.name
            for path in sorted(MIGRATIONS.glob("*.sql"))
            if re.search(
                rf"ALTER TABLE\s+{DECISION_TABLE}\s+ALTER COLUMN trace_id SET DEFAULT",
                path.read_text(encoding="utf-8", errors="replace"),
                re.S,
            )
        ]
        self.assertEqual(defaulted, [], f"a migration gives trace_id a default: {defaulted}")


if __name__ == "__main__":
    unittest.main()
