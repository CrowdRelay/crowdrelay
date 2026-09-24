#!/usr/bin/env python3
"""Every column a `FromRow` struct needs is one its query actually returns.

`sqlx::query_as::<_, Row>(sql)` decodes by column *name*. A struct field with no
matching output column fails at runtime with `no column found for name: …` —
after compiling, linting, passing every unit test, and preparing clean, because
`PREPARE` only checks that the SQL is valid, not that it returns what the Rust
side reads.

The door view (`concert_qr/scan_view.rs`) did exactly this. It reused the
timeline's `TimelineEventRow`, the timeline grew `venue_address`, the
counterparty columns, `place_event_id` and `booking_opportunity_id`, and the
scan query kept selecting the original seven. Every show's door page answered
503 for as long as that was true, and nothing in `just ci` noticed.

How this checks it: for each `query_as::<_, Name>(<literal SQL>)` whose `Name`
resolves to one `#[derive(FromRow)]` struct, ask PostgreSQL for the query's
output columns with psql's `\\gdesc` (it describes a parameterised statement
without executing it) and compare them with the struct's fields, honouring
`#[sqlx(rename = …)]`, `#[sqlx(default)]`, `#[sqlx(skip)]` and a struct-level
`rename_all`. A struct with a `#[sqlx(flatten)]` field is skipped — its columns
come from another struct. Tuples decode by position and are out of scope.

Skips when no local database container is running, like
`sql-result-types.py`, whose container lookup and foreign-relation list it
shares. Run `just migrate` first: a stale schema describes stale columns.
"""
from __future__ import annotations

import importlib.util
import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

_spec = importlib.util.spec_from_file_location("sql_result_types", ROOT / "scripts/sql-result-types.py")
_types = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_types)

QUERY_AS = re.compile(
    r'query_as::<\s*_\s*,\s*([A-Z][A-Za-z0-9_]*)\s*>\s*\(\s*(?:r#"(.*?)"#|"((?:[^"\\]|\\.)*)")',
    re.S,
)


def snake_to_camel(name: str) -> str:
    head, *rest = name.split("_")
    return head + "".join(part.capitalize() for part in rest)


def crate_of(path: Path) -> str:
    parts = path.relative_to(ROOT).parts
    return parts[1] if len(parts) > 1 else ""


STRUCT_HEAD = re.compile(
    r"((?:#\[[^\n]*\]\s*)*)(?:pub(?:\([^)]*\))?\s+)?struct\s+([A-Z][A-Za-z0-9_]*)\s*(?:<[^>{]*>)?\s*\{",
)


def braced_body(text: str, open_at: int) -> str:
    """The text between the `{` at `open_at` and its matching `}`."""
    depth = 0
    for index in range(open_at, len(text)):
        if text[index] == "{":
            depth += 1
        elif text[index] == "}":
            depth -= 1
            if depth == 0:
                return text[open_at + 1 : index]
    return ""


def fields_of(body: str) -> list[tuple[str, str]]:
    """(attributes, name) per field, splitting on commas outside brackets."""
    body = re.sub(r"//[^\n]*", "", body)
    parts, depth, current = [], 0, []
    for char in body:
        if char in "<([{":
            depth += 1
        elif char in ">)]}":
            depth -= 1
        if char == "," and depth == 0:
            parts.append("".join(current))
            current = []
        else:
            current.append(char)
    parts.append("".join(current))
    out = []
    for part in parts:
        attrs = " ".join(re.findall(r"#\[[^\]]*\]", part))
        rest = re.sub(r"#\[[^\]]*\]", "", part).strip()
        match = re.match(r"(?:pub(?:\([^)]*\))?\s+)?(?:r#)?([a-z_][a-z0-9_]*)\s*:(?!:)", rest)
        if match:
            out.append((attrs, match.group(1)))
    return out


def load_structs() -> dict[str, list[tuple[Path, dict]]]:
    """FromRow structs by name, with their required and optional columns."""
    found: dict[str, list[tuple[Path, dict]]] = {}
    for path in sorted((ROOT / "crates").rglob("*.rs")):
        text = path.read_text(errors="ignore")
        for match in STRUCT_HEAD.finditer(text):
            attrs, name = match.groups()
            if "FromRow" not in attrs:
                continue
            body = braced_body(text, match.end() - 1)
            rename_all = re.search(r'#\[sqlx\([^\]]*rename_all\s*=\s*"([^"]+)"', attrs)
            required, optional, flatten = [], [], False
            for field_attrs, fname in fields_of(body):
                sqlx_attrs = " ".join(re.findall(r"#\[sqlx\(([^\]]*)\)\]", field_attrs))
                if "flatten" in sqlx_attrs:
                    flatten = True
                if re.search(r"\bskip\b", sqlx_attrs):
                    continue
                renamed = re.search(r'rename\s*=\s*"([^"]+)"', sqlx_attrs)
                column = renamed.group(1) if renamed else fname
                if not renamed and rename_all and rename_all.group(1) == "camelCase":
                    column = snake_to_camel(fname)
                (optional if re.search(r"\bdefault\b", sqlx_attrs) else required).append(column)
            found.setdefault(name, []).append((path, {"required": required, "optional": optional, "flatten": flatten}))
    return found


def query_sites(structs: dict) -> list[tuple[str, int, str, dict]]:
    """(file, line, sql, struct) for every literal query_as with a resolvable struct."""
    sites = []
    for path in sorted((ROOT / "crates").rglob("*.rs")):
        relative = path.relative_to(ROOT).as_posix()
        if _types.is_test_source(relative):
            continue
        text = path.read_text(errors="ignore")
        for match in QUERY_AS.finditer(text):
            name = match.group(1)
            sql = (match.group(2) if match.group(2) is not None else match.group(3)).strip()
            if match.group(3) is not None:
                sql = sql.replace('\\"', '"').replace("\\n", "\n")
            candidates = structs.get(name, [])
            same_file = [c for c in candidates if c[0] == path]
            same_crate = [c for c in candidates if crate_of(c[0]) == crate_of(path)]
            chosen = same_file or same_crate or (candidates if len(candidates) == 1 else [])
            if len(chosen) != 1:
                continue  # Unknown or ambiguous name — nothing honest to compare.
            shape = chosen[0][1]
            if shape["flatten"] or _types.names_a_foreign_relation(sql):
                continue
            line = text[: match.start()].count("\n") + 1
            sites.append((relative, line, sql, shape))
    return sites


def describe(container: str, sites: list) -> dict[int, list[str]]:
    script = ["\\set ON_ERROR_STOP 0"]
    for index, (_, _, sql, _) in enumerate(sites):
        script.append(f"\\echo BEGIN_{index}")
        script.append(sql.rstrip().rstrip(";") + " \\gdesc")
        script.append(f"\\echo END_{index}")
    result = _types.psql("\n".join(script), container)
    columns: dict[int, list[str]] = {}
    current, rows = None, []
    for line in result.stdout.splitlines():
        if line.startswith("BEGIN_"):
            current, rows = int(line[6:]), []
        elif line.startswith("END_") and current is not None:
            if rows:
                columns[current] = rows
            current = None
        elif current is not None and "|" in line:
            rows.append(line.split("|", 1)[0])
    return columns


class SqlRowShapes(unittest.TestCase):
    def setUp(self) -> None:
        self.container = _types.find_container()
        if not self.container:
            self.skipTest("no local crowdrelay-postgres-1 container; run `just db-up`")

    def test_every_from_row_field_has_a_column(self) -> None:
        sites = query_sites(load_structs())
        described = describe(self.container, sites)
        # A parser that resolves nothing, or a psql that describes nothing,
        # would pass the comparison below by comparing nothing.
        self.assertGreater(len(sites), 200, f"only {len(sites)} query_as sites resolved")
        self.assertGreater(len(described), 150, f"psql described only {len(described)} queries")
        missing = []
        for index, (path, line, _, shape) in enumerate(sites):
            columns = described.get(index)
            if columns is None:
                continue  # Did not describe standalone; sql-result-types owns that.
            absent = [field for field in shape["required"] if field not in columns]
            if absent:
                missing.append(f"{path}:{line} reads {absent}; the query returns {columns}")
        self.assertEqual(
            missing,
            [],
            "these structs read columns their query does not return — each fails "
            "at runtime with `no column found for name`:\n  " + "\n  ".join(missing),
        )


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    container = _types.find_container()
    if container and result.wasSuccessful():
        sites = query_sites(load_structs())
        print(f"SQL_ROW_SHAPES=PASS sites={len(sites)} described={len(describe(container, sites))}")
    else:
        print("SQL_ROW_SHAPES=" + ("PASS" if result.wasSuccessful() else "FAIL"))
    sys.exit(0 if result.wasSuccessful() else 1)
