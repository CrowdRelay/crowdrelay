#!/usr/bin/env python3
"""A `query_scalar` into a float must select a float.

The workspace uses runtime sqlx queries with no compile-time SQL checking, so
a column whose SQL type does not match the Rust type it is decoded into
compiles, lints and unit-tests clean, then fails on a live server every single
time it runs.

That is not hypothetical. `agent_run_outcome_quality_1h` selected
`CASE WHEN ... THEN 1.0 ELSE 0.0 END` into an `f64`. Postgres types a bare
`1.0` literal as NUMERIC and `f64` is FLOAT8, so every attempt failed with:

    mismatched types; Rust type `f64` (as SQL type `FLOAT8`)
    is not compatible with SQL type `NUMERIC`

The measurement could never resolve. It was retried, failed identically, and
degraded the autopilot cycle that attempted it — for as long as it was
deployed, with nothing failing in CI to say so.

`count(*)` has the same shape: it returns BIGINT, not FLOAT8.

The rule: a `query_scalar::<_, f64>` must make its float-ness explicit, with
`::double precision` or `::float8` somewhere in the statement. This is a
lexical check, not a type checker — it cannot prove the cast is on the right
column. It does force the author to have thought about the column's SQL type,
which is the step that was skipped.

`just test-postgres` remains the real check, against a real schema.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# `query_scalar::<_, f64>(` followed by the SQL string literal, raw or plain.
QUERY = re.compile(
    r"query_scalar::<\s*_\s*,\s*f64\s*>\s*\(\s*(?:r#\"(?P<raw>.*?)\"#|\"(?P<plain>(?:[^\"\\]|\\.)*)\")",
    re.DOTALL,
)
FLOAT_CAST = ("double precision", "::float8", "::FLOAT8")


def offenders() -> list[tuple[Path, int, str]]:
    found: list[tuple[Path, int, str]] = []
    for path in sorted(ROOT.glob("crates/*/src/**/*.rs")):
        source = path.read_text(encoding="utf-8")
        if "query_scalar" not in source:
            continue
        for match in QUERY.finditer(source):
            statement = match.group("raw") or match.group("plain") or ""
            if any(cast in statement for cast in FLOAT_CAST):
                continue
            line = source[: match.start()].count("\n") + 1
            first = " ".join(part.strip() for part in statement.strip().splitlines()[:2])
            found.append((path.relative_to(ROOT), line, first[:120]))
    return found


def main() -> int:
    found = offenders()
    if found:
        print("SQL_SCALAR_TYPES=FAIL", file=sys.stderr)
        print(
            "A query_scalar into f64 must cast its column to double precision.\n"
            "Postgres types `1.0` as NUMERIC and `count(*)` as BIGINT; neither\n"
            "decodes into f64, and nothing catches it until the query runs.",
            file=sys.stderr,
        )
        for path, line, statement in found:
            print(f"  {path}:{line}  {statement}", file=sys.stderr)
        return 1
    print("SQL_SCALAR_TYPES=PASS checks=query_scalar-f64")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
