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


def sweep(container: str) -> tuple[list[tuple[str, int, list[int], list[str], str]], int, int]:
    """Prepares each query and returns those with a NUMERIC output column."""
    candidates = queries()
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
    for line in result.stdout.splitlines():
        if not line.startswith("TYPES|"):
            continue
        prepared += 1
        _, index, types = line.split("|", 2)
        columns = [t.strip() for t in types.strip().strip("{}").split(",")]
        positions = [i for i, t in enumerate(columns) if t == "numeric"]
        if not positions:
            continue
        path, line_no, sql = candidates[int(index)]
        reasons = [why for token, why in NUMERIC_SOURCES if token in sql.upper()]
        offenders.append((path, line_no, positions, reasons, sql))
    return offenders, prepared, len(candidates)


class SqlResultTypes(unittest.TestCase):
    def setUp(self):
        self.container = find_container()
        if not self.container:
            self.skipTest(
                "no local crowdrelay-postgres-1 container; run `just db-up` — "
                "`just test-postgres` and the deploy both have one"
            )

    def test_no_query_returns_numeric(self):
        offenders, prepared, total = sweep(self.container)
        # A sweep that prepared almost nothing proves nothing. Some queries are
        # built with format! and cannot be prepared standalone; most can.
        self.assertGreater(
            prepared,
            total * 0.8,
            f"only {prepared} of {total} queries could be prepared; the sweep is "
            f"not covering enough to be meaningful",
        )
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
    offenders, prepared, total = sweep(container)
    if offenders:
        print("SQL_RESULT_TYPES=FAIL")
        for path, line, positions, reasons, _ in offenders:
            print(f"  {path}:{line} numeric at column {positions}")
            for reason in reasons:
                print(f"      likely: {reason}")
        return 1
    print(f"SQL_RESULT_TYPES=PASS prepared={prepared}/{total} numeric_columns=0")
    return 0


if __name__ == "__main__":
    if "--unittest" in sys.argv:
        sys.argv.remove("--unittest")
        unittest.main()
    raise SystemExit(main())
