#!/usr/bin/env python3
"""A `query_scalar` must select the SQL type it decodes into.

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

Three rules, each one a decode that Postgres will refuse:

- `f64` wants FLOAT8. Say so with `::double precision` or `::float8`.
- `i32` wants INT4, and both `count()` and `sum()` return something wider.
- `i64` wants INT8, and `avg()` returns NUMERIC.

This is a lexical check, not a type checker — it cannot prove the cast is on
the right column. It does force the author to have thought about the column's
SQL type, which is the step that was skipped.

`query_as` is deliberately not covered. Its columns are decoded into a struct
declared elsewhere, so knowing whether a projection matches its field would
mean resolving types across files — and the lexical approximations tried here
flagged 50 float literals sitting inside `WHERE` arithmetic, none of which is
decoded into anything. A gate that noisy gets baselined away, which is worse
than no gate. `just test-postgres` covers those, against a real schema.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# `query_scalar::<_, f64>(` followed by the SQL string literal, raw or plain.
QUERY = re.compile(
    r"query_scalar::<\s*_\s*,\s*(?P<ty>Option<f64>|f64|Option<i32>|i32|Option<i64>|i64)\s*>"
    r"\s*\(\s*(?:r#\"(?P<raw>.*?)\"#|\"(?P<plain>(?:[^\"\\]|\\.)*)\")",
    re.DOTALL,
)
FLOAT_CAST = ("double precision", "::float8", "::FLOAT8")
WIDENING = re.compile(r"\b(count|sum)\s*\(", re.IGNORECASE)
AVERAGE = re.compile(r"\bavg\s*\(", re.IGNORECASE)


def offenders() -> list[tuple[Path, int, str]]:
    found: list[tuple[Path, int, str]] = []
    for path in sorted(ROOT.glob("crates/*/src/**/*.rs")):
        source = path.read_text(encoding="utf-8")
        if "query_scalar" not in source:
            continue
        for match in QUERY.finditer(source):
            statement = match.group("raw") or match.group("plain") or ""
            rust_type = match.group("ty")
            lowered = statement.lower()
            if "f64" in rust_type:
                # FLOAT8. A bare `1.0` is NUMERIC and `count(*)` is BIGINT.
                if any(cast in statement for cast in FLOAT_CAST):
                    continue
            elif "i32" in rust_type:
                # INT4. `count()` and `sum()` both return something wider.
                if not WIDENING.search(statement) or "::int" in lowered:
                    continue
            else:
                # INT8. `avg()` returns NUMERIC.
                if not AVERAGE.search(statement) or "::bigint" in lowered:
                    continue
            line = source[: match.start()].count("\n") + 1
            first = " ".join(part.strip() for part in statement.strip().splitlines()[:2])
            found.append((path.relative_to(ROOT), line, f"[{rust_type}] {first}"[:130]))
    return found


def main() -> int:
    found = offenders()
    if found:
        print("SQL_SCALAR_TYPES=FAIL", file=sys.stderr)
        print(
            "A query_scalar must select the SQL type it decodes into.\n"
            "  f64 wants FLOAT8 -- `1.0` is NUMERIC, `count(*)` is BIGINT.\n"
            "  i32 wants INT4   -- `count()` and `sum()` are both wider.\n"
            "  i64 wants INT8   -- `avg()` is NUMERIC.\n"
            "Nothing catches any of these until the query runs.",
            file=sys.stderr,
        )
        for path, line, statement in found:
            print(f"  {path}:{line}  {statement}", file=sys.stderr)
        return 1
    print("SQL_SCALAR_TYPES=PASS checks=query_scalar-f64,i32,i64")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
