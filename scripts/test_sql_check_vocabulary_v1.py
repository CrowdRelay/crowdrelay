#!/usr/bin/env python3
"""A literal written into a CHECK-constrained column is a value the CHECK allows.

`audit_events.actor_kind` accepts member, system and service. The synesthesia
leaderboard unpublish wrote `'fan'` there, inside the same transaction as the
unpublish itself, so the constraint violation rolled the fan's privacy request
back every time — and nothing but a real fan pressing the button would ever
have run that INSERT.

This reads every `CHECK (column = ANY (ARRAY[...]))` from the live schema and
compares it with the string literals the code writes: `UPDATE table SET
column = 'value'` and `INSERT INTO table (...) VALUES (...)` with a literal in
that column's position, in every raw SQL literal outside tests. Composite
constraints (`status = 'x' AND at IS NOT NULL`) are not modelled; only the pure
vocabulary form is, and several pure constraints on one column intersect.

The second test reads the same vocabularies against comparisons: `alias.column
= 'value'`, `<>`, and `IN ('a', 'b')`. A value the CHECK forbids can never be
stored, so a filter on it is dead — `fans.status = 'closed'` sat in three
audience queries, where the vocabulary is pending/active/unsubscribed/
suppressed/merged, so the "account closed" activation state could not occur
and merged tombstones listed as live fans. An alias bound to two tables in one
statement, or an unqualified column that more than one named table has, is
skipped rather than guessed at.

Skips without a local database container, like `sql-result-types.py`.
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

PURE = re.compile(
    r"CHECK \(\(\(?(\w+)\)?(?:::text)? = ANY \(\(?ARRAY\[(.*?)\]\)?(?:::text\[\])?\)\)\)"
)
UPDATE = re.compile(
    r"UPDATE\s+(\w+)(?:\s+(?:AS\s+)?\w+)?\s+SET\s+(.*?)(?=\bWHERE\b|\bRETURNING\b|\bFROM\b|$)",
    re.S | re.I,
)
INSERT = re.compile(
    r"INSERT\s+INTO\s+(\w+)\s*\(([^)]*)\)\s*VALUES\s*\((.*?)\)\s*(?:ON\b|RETURNING\b|$)",
    re.S | re.I,
)


def vocabularies(container: str) -> dict[str, dict[str, set[str]]]:
    result = _types.psql(
        "SELECT conrelid::regclass::text, pg_get_constraintdef(oid) "
        "FROM pg_constraint WHERE contype = 'c';",
        container,
    )
    allowed: dict[str, dict[str, set[str]]] = {}
    for line in result.stdout.splitlines():
        table, definition = line.split("|", 1)
        match = PURE.fullmatch(definition.strip())
        if not match:
            continue
        column, values = match.group(1), set(re.findall(r"'([^']*)'", match.group(2)))
        columns = allowed.setdefault(table, {})
        columns[column] = columns[column] & values if column in columns else values
    return allowed


def split_values(values: str) -> list[str]:
    parts, depth, current = [], 0, []
    for char in values:
        if char == "(":
            depth += 1
        elif char == ")":
            depth -= 1
        if char == "," and depth == 0:
            parts.append("".join(current).strip())
            current = []
        else:
            current.append(char)
    parts.append("".join(current).strip())
    return parts


def violations(allowed: dict) -> tuple[list[str], int]:
    found, checked = [], 0
    for path in sorted((ROOT / "crates").rglob("*.rs")):
        relative = path.relative_to(ROOT).as_posix()
        if _types.is_test_source(relative):
            continue
        text = path.read_text(errors="ignore")
        for literal in _types.RAW_LITERAL.finditer(text):
            sql = re.sub(r"--[^\n]*", "", literal.group(1))
            line = text[: literal.start()].count("\n") + 1
            for match in UPDATE.finditer(sql):
                table = match.group(1)
                for column, value in re.findall(r"(\w+)\s*=\s*'([^']*)'", match.group(2)):
                    if column in allowed.get(table, {}):
                        checked += 1
                        if value not in allowed[table][column]:
                            found.append(f"{relative}:{line} UPDATE {table}.{column} = '{value}'")
            for match in INSERT.finditer(sql):
                table = match.group(1)
                columns = [c.strip() for c in match.group(2).split(",")]
                values = split_values(match.group(3))
                if len(columns) != len(values):
                    continue
                for column, value in zip(columns, values):
                    literal_value = re.fullmatch(r"'([^']*)'(?:::\w+)?", value)
                    if literal_value and column in allowed.get(table, {}):
                        checked += 1
                        if literal_value.group(1) not in allowed[table][column]:
                            found.append(f"{relative}:{line} INSERT {table}.{column} = '{literal_value.group(1)}'")
    return found, checked


RELATION = re.compile(r"\b(?:FROM|JOIN|UPDATE|INTO)\s+(?:ONLY\s+)?(\w+)(?:\s+(?:AS\s+)?(\w+))?", re.I)
COMPARE = re.compile(r"(?<![\w.'])(?:(\w+)\.)?(\w+)\s*(?:=|<>|!=)\s*'([^']*)'", re.I)
IN_LIST = re.compile(r"(?<![\w.'])(?:(\w+)\.)?(\w+)\s+(?:NOT\s+)?IN\s*\(\s*('[^)]*')\s*\)", re.I)
KEYWORDS = {
    "where", "on", "and", "or", "join", "left", "right", "inner", "full", "cross", "lateral",
    "set", "select", "group", "order", "limit", "using", "natural", "outer", "returning",
    "values", "as", "for", "union", "window", "having",
}


def table_columns(container: str) -> dict[str, set[str]]:
    result = _types.psql(
        "SELECT table_name, column_name FROM information_schema.columns "
        "WHERE table_schema = 'public';",
        container,
    )
    columns: dict[str, set[str]] = {}
    for line in result.stdout.splitlines():
        table, column = line.split("|", 1)
        columns.setdefault(table, set()).add(column)
    return columns


def dead_filters(allowed: dict, columns: dict) -> tuple[list[str], int]:
    found, checked = [], 0
    for path in sorted((ROOT / "crates").rglob("*.rs")):
        relative = path.relative_to(ROOT).as_posix()
        if _types.is_test_source(relative):
            continue
        text = path.read_text(errors="ignore")
        for literal in _types.RAW_LITERAL.finditer(text):
            sql = re.sub(r"--[^\n]*", "", literal.group(1))
            line = text[: literal.start()].count("\n") + 1
            bound: dict[str, set[str]] = {}
            tables: set[str] = set()
            for relation in RELATION.finditer(sql):
                table, alias = relation.group(1).lower(), (relation.group(2) or "").lower()
                if table in KEYWORDS:
                    continue
                tables.add(table)
                bound.setdefault(table, set()).add(table)
                if alias and alias not in KEYWORDS:
                    bound.setdefault(alias, set()).add(table)
            comparisons = [(m.group(1), m.group(2), [m.group(3)]) for m in COMPARE.finditer(sql)]
            comparisons += [
                (m.group(1), m.group(2), re.findall(r"'([^']*)'", m.group(3)))
                for m in IN_LIST.finditer(sql)
            ]
            for qualifier, column, values in comparisons:
                column = column.lower()
                if qualifier:
                    owners = bound.get(qualifier.lower(), set())
                else:
                    owners = {table for table in tables if column in columns.get(table, set())}
                if len(owners) != 1:
                    continue
                table = next(iter(owners))
                vocabulary = allowed.get(table, {}).get(column)
                if not vocabulary:
                    continue
                checked += 1
                impossible = [value for value in values if value not in vocabulary]
                if impossible:
                    found.append(f"{relative}:{line} {table}.{column} compared with {impossible}")
    return found, checked


class SqlCheckVocabulary(unittest.TestCase):
    def setUp(self) -> None:
        self.container = _types.find_container()
        if not self.container:
            self.skipTest("no local crowdrelay-postgres-1 container; run `just db-up`")

    def test_every_written_literal_passes_its_check(self) -> None:
        allowed = vocabularies(self.container)
        self.assertGreater(len(allowed), 100, "constraint parse collapsed")
        found, checked = violations(allowed)
        self.assertGreater(checked, 150, f"only {checked} literal writes were checked")
        self.assertEqual(
            found,
            [],
            "these writes violate the column's CHECK and roll back their "
            "transaction:\n  " + "\n  ".join(found),
        )

    def test_every_compared_literal_can_exist(self) -> None:
        allowed = vocabularies(self.container)
        found, checked = dead_filters(allowed, table_columns(self.container))
        self.assertGreater(checked, 1000, f"only {checked} literal comparisons were checked")
        self.assertEqual(
            found,
            [],
            "these comparisons name a value the column's CHECK forbids, so "
            "they can never match:\n  " + "\n  ".join(found),
        )


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    print("SQL_CHECK_VOCABULARY=" + ("PASS" if result.wasSuccessful() else "FAIL"))
    sys.exit(0 if result.wasSuccessful() else 1)
