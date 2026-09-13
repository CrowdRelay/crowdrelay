#!/usr/bin/env python3
"""Ask PostgreSQL for every query's output types, and refuse NUMERIC.

CrowdRelay uses runtime `sqlx::query`/`query_as` by choice, and CLAUDE.md names
the consequence: "A query naming a table that does not exist compiles, lints and
tests clean, then fails on first request." `scripts/test_sql_identifiers_v1.py`
covers relation names. This covers result types, which broke production.

On 2026-09-13 every autopilot cycle degraded for two hours with:

    error occurred while decoding column 2: mismatched types;
    Rust type `f64` (as SQL type `FLOAT8`) is not compatible with SQL type `NUMERIC`

The cause was `EXTRACT(EPOCH FROM ...) / 3600.0` in
`growth_intelligence/worker_signals.rs`. PostgreSQL 14 changed `EXTRACT` to
return `numeric` where it had returned `double precision`, dividing by a numeric
literal keeps it numeric, and the row type decodes `f64`. It compiled, linted and
passed 1684 tests, because no test executes that query with a row in it — it
reads operator approvals, and the loop had never produced anything to approve.
It started failing the first time the brain got that far.

`PREPARE` answers this without executing anything or needing any data:
`pg_prepared_statements.result_types` is exactly what sqlx will try to decode.

Why NUMERIC specifically. Nothing in this schema stores `numeric` except
`merch_coupons.discount_percent`, so a NUMERIC output column is almost always an
accident of an expression — `EXTRACT`, or `SUM`/`AVG` over a `bigint`, both of
which this codebase otherwise casts. Rust has no NUMERIC-compatible primitive in
use here, so every one of them is a decode failure waiting for its first row.

Skips when no database is reachable, the same way the cross-repo gates skip
without their sibling checkout. `just test-postgres` and the deploy both have one.
"""
from __future__ import annotations

import json
import re
import subprocess
import sys
import json
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

# Expressions that produce NUMERIC and are always a mistake here. Listed so the
# failure message can name the likely culprit instead of only the column index.
NUMERIC_SOURCES = (
    ("EXTRACT(", "EXTRACT returns numeric on PostgreSQL 14+; cast to ::double precision"),
    ("SUM(", "SUM over a bigint returns numeric; cast to ::bigint"),
    ("AVG(", "AVG over an integer returns numeric; cast to ::double precision"),
)


BASELINE = Path(__file__).with_suffix(".json")


def baseline() -> dict | None:
    if not BASELINE.exists():
        return None
    return json.loads(BASELINE.read_text())


def new_unprepared(unprepared: list[tuple[str, int]]) -> list[str]:
    """Only the queries that did not prepare and are not in the baseline.

    A count on its own told you something broke and then listed all fifty-eight
    queries that never prepared, burying the one that matters. Line numbers move,
    so a shifted entry can show up here spuriously — acceptable in a message that
    already says the count dropped, and far better than fifty-eight rows.
    """
    known = set((baseline() or {}).get("unprepared", []))
    return [f"{path}:{line}" for path, line in unprepared if f"{path}:{line}" not in known]


def real_errors(stderr: str) -> list[str]:
    """psql errors, without the follow-on noise.

    Every failed PREPARE leaves its transaction aborted, so each real error is
    trailed by a "current transaction is aborted" line that says nothing.
    """
    return [
        line
        for line in stderr.splitlines()
        if "ERROR" in line and "current transaction is aborted" not in line
    ]


def baseline_minimum() -> int | None:
    """The prepared count this schema is known to reach.

    A floor of `total * 0.8` was not enough. Breaking one query took the count
    from 679 to 678 — 92% of 736, comfortably above the ratio — and the script
    printed PASS. The single most valuable thing it can catch is a query that
    will not prepare, and it was treating that as "never mind".

    Shrinking is a deliberate act: pass `--write-baseline` after removing
    queries on purpose.
    """
    recorded = baseline()
    return None if recorded is None else int(recorded["minimum_prepared"])


def psql(sql: str, container: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["docker", "exec", "-i", container,
         "psql", "-U", "crowdrelay", "-d", "crowdrelay", "-At", "-F", "|"],
        input=sql, capture_output=True, text=True, timeout=300,
    )


def find_container() -> str | None:
    try:
        listed = subprocess.run(
            ["docker", "ps", "--format", "{{.Names}}"],
            capture_output=True, text=True, timeout=30,
        ).stdout.split()
    except (OSError, subprocess.SubprocessError):
        return None
    for name in listed:
        if name.endswith("crowdrelay-postgres-1"):
            return name
    return None


# Relations another service owns and creates. They are absent from a freshly
# created development database, and a query naming one then fails to PREPARE for
# a reason that has nothing to do with the query. Without this the prepared-count
# ratchet fails on every clean checkout, which is how a gate teaches people to
# ignore it. Kept in step with `test_sql_identifiers_v1.py`, which carries the
# same list for the same reason.
FOREIGN_RELATIONS = (
    "agent_service_tasks",
    "agent_service_reddit_cookies",
    "agent_service_credentials",
)


def names_a_foreign_relation(sql: str) -> bool:
    lowered = sql.lower()
    return any(relation in lowered for relation in FOREIGN_RELATIONS)


def queries() -> list[tuple[str, int, str]]:
    """Every raw SQL literal that returns rows, with where it was written."""
    found: list[tuple[str, int, str]] = []
    literal = re.compile(r'r#"(.*?)"#', re.S)
    for path in sorted((ROOT / "crates").rglob("*.rs")):
        if "/tests/" in str(path):
            continue
        text = path.read_text(errors="ignore")
        for match in literal.finditer(text):
            sql = match.group(1).strip()
            head = sql.lower()
            # RETURNING clauses are writes; PREPARE would execute nothing but the
            # sweep runs them in a rolled-back transaction either way, and
            # skipping them keeps the sweep obviously read-only.
            if not head.startswith(("select", "with")) or "returning" in head:
                continue
            line = text[: match.start()].count("\n") + 1
            found.append((str(path.relative_to(ROOT)), line, sql))
    return found


def sweep(container: str) -> tuple[list, int, int, list, str]:
    """Prepares each query and returns those with a NUMERIC output column."""
    # A query naming a relation another service owns cannot prepare against a
    # database that service has not touched. Excluding them keeps the prepared
    # count a property of this repository rather than of what else has run.
    candidates = [
        candidate for candidate in queries() if not names_a_foreign_relation(candidate[2])
    ]
    script: list[str] = ["\\set ON_ERROR_STOP 0"]
    for index, (_, _, sql) in enumerate(candidates):
        script.append(
            f"BEGIN; PREPARE q{index} AS {sql}; "
            f"SELECT 'TYPES', {index}, result_types FROM pg_prepared_statements "
            f"WHERE name='q{index}'; ROLLBACK;"
        )
    result = psql("\n".join(script), container)
    offenders = []
    prepared = 0
    seen = set()
    for line in result.stdout.splitlines():
        if not line.startswith("TYPES|"):
            continue
        prepared += 1
        _, index, types = line.split("|", 2)
        seen.add(int(index))
        columns = [t.strip() for t in types.strip().strip("{}").split(",")]
        positions = [i for i, t in enumerate(columns) if t == "numeric"]
        if not positions:
            continue
        path, line_no, sql = candidates[int(index)]
        reasons = [why for token, why in NUMERIC_SOURCES if token in sql.upper()]
        offenders.append((path, line_no, positions, reasons, sql))
    # Every candidate that produced no TYPES row. Some cannot be prepared
    # standalone — a fragment built with format!, a multi-statement body — and
    # those are the baseline. A NEW one is the thing this script exists to
    # catch: a query naming a column that does not exist is exactly the failure
    # mode of runtime SQL, and it was silently counted as "not prepared".
    unprepared = [
        (candidates[i][0], candidates[i][1])
        for i in range(len(candidates))
        if i not in seen
    ]
    return offenders, prepared, len(candidates), unprepared, result.stderr


class SqlResultTypes(unittest.TestCase):
    def setUp(self):
        self.container = find_container()
        if not self.container:
            self.skipTest(
                "no local crowdrelay-postgres-1 container; run `just db-up` — "
                "`just test-postgres` and the deploy both have one"
            )

    def test_every_query_that_used_to_prepare_still_does(self):
        """The count is a ratchet, not a ratio.

        A query that will not prepare is the whole failure mode of runtime SQL,
        and this script used to skip it. Breaking one query moved the count from
        679 to 678, which cleared the old `> total * 0.8` floor and printed PASS.
        """
        _, prepared, total, unprepared, stderr = sweep(self.container)
        minimum = baseline_minimum()
        if minimum is None:
            self.skipTest("no scripts/sql-result-types.json baseline recorded")
        self.assertGreaterEqual(
            prepared,
            minimum,
            f"{minimum - prepared} query/queries stopped preparing "
            f"({prepared}/{total}, baseline {minimum}). Newly unprepared:\n"
            + "\n".join(f"  {entry}" for entry in new_unprepared(unprepared))
            + "\n\npsql said:\n"
            + "\n".join(f"  {line}" for line in real_errors(stderr)[-8:])
            + "\n\nIf queries were removed on purpose, re-record with "
            "`python3 scripts/sql-result-types.py --write-baseline`.",
        )

    def test_no_query_returns_numeric(self):
        offenders, prepared, total, _, _ = sweep(self.container)
        if offenders:
            report = []
            for path, line, positions, reasons, _ in offenders:
                report.append(f"  {path}:{line} numeric at column {positions}")
                for reason in reasons:
                    report.append(f"      likely: {reason}")
            self.fail(
                "these queries return NUMERIC, which no Rust type here decodes:\n"
                + "\n".join(report)
                + "\n\nNothing in this schema stores numeric except "
                "merch_coupons.discount_percent, so each of these is an "
                "expression that needs an explicit cast. It will compile, lint "
                "and pass every test, then fail on the first row."
            )


def main() -> int:
    container = find_container()
    if not container:
        print("SQL_RESULT_TYPES=SKIP reason=no-local-database")
        return 0
    offenders, prepared, total, unprepared, stderr = sweep(container)
    if "--write-baseline" in sys.argv:
        BASELINE.write_text(
            json.dumps(
                {
                    "minimum_prepared": prepared,
                    # Recorded so a failure can name what is NEW rather than
                    # listing every query that has never prepared.
                    "unprepared": sorted(f"{path}:{line}" for path, line in unprepared),
                },
                indent=2,
            )
            + "\n"
        )
        print(f"SQL_RESULT_TYPES=BASELINE minimum_prepared={prepared}")
        return 0
    minimum = baseline_minimum()
    if minimum is not None and prepared < minimum:
        print(f"SQL_RESULT_TYPES=FAIL prepared={prepared}/{total} baseline={minimum}")
        print("  newly unprepared; one names something that does not exist:")
        for entry in new_unprepared(unprepared):
            print(f"    {entry}")
        for line in real_errors(stderr)[-8:]:
            print(f"    psql: {line}")
        print("  if queries were removed on purpose: --write-baseline")
        return 1
    if offenders:
        print("SQL_RESULT_TYPES=FAIL")
        for path, line, positions, reasons, _ in offenders:
            print(f"  {path}:{line} numeric at column {positions}")
            for reason in reasons:
                print(f"      likely: {reason}")
        return 1
    print(
        f"SQL_RESULT_TYPES=PASS prepared={prepared}/{total} "
        f"baseline={minimum} numeric_columns=0"
    )
    return 0


if __name__ == "__main__":
    if "--unittest" in sys.argv:
        sys.argv.remove("--unittest")
        unittest.main()
    raise SystemExit(main())
