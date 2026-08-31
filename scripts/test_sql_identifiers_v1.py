#!/usr/bin/env python3
"""Check that runtime SQL only names tables the migrations actually create.

The workspace builds without DATABASE_URL by design — runtime `sqlx::query`
instead of the compile-time macros — which buys fast, offline builds at the
cost of the one invariant the macros gave for free: that a query references
real relations.

The gap is not theoretical. A north-star query shipped reading
`growth_metric_series_points` and `growth_metric_series`; the real tables are
`viryaos_growth_metric_points` and `viryaos_growth_metric_series`. It compiled,
passed clippy, passed the full test suite and passed every contract script.
It would have failed on the first request that reached it.

This recovers the cheap half of that invariant: every identifier appearing
after FROM, JOIN, INSERT INTO, UPDATE or DELETE FROM in a Rust string literal
must be a table or view some migration creates, a CTE bound in the same query,
or an alias. It does not typecheck columns — `just test-postgres` does that
against a real schema.
"""
from __future__ import annotations

import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MIGRATIONS = ROOT / "migrations"
CRATES = ROOT / "crates"

CREATE = re.compile(
    r"CREATE\s+(?:OR\s+REPLACE\s+)?(?:UNLOGGED\s+|TEMP(?:ORARY)?\s+)?"
    r"(?:TABLE|VIEW|MATERIALIZED\s+VIEW)\s+"
    r"(?:IF\s+NOT\s+EXISTS\s+)?(?:public\.)?\"?(\w+)\"?",
    re.IGNORECASE,
)
RENAME = re.compile(r"ALTER\s+TABLE\s+(?:public\.)?\"?(\w+)\"?\s+RENAME\s+TO\s+\"?(\w+)\"?", re.IGNORECASE)
# The identifier is anchored with \b on both sides. A lookahead here would let
# the group backtrack a character to satisfy it, silently matching
# `outbox_event` inside `outbox_events (` — the "call, not relation" case is
# handled after the match instead, where it cannot corrupt the name.
REFERENCE = re.compile(
    r"\b(?:FROM|JOIN|INSERT\s+INTO|UPDATE|DELETE\s+FROM)\s+(?:ONLY\s+)?(?:public\.)?"
    r"\"?\b([a-z_][a-z0-9_]*)\b\"?",
    re.IGNORECASE,
)
# `a IS NOT DISTINCT FROM b.col` compares values; its FROM names no relation.
DISTINCT_FROM = re.compile(r"\bIS\s+(?:NOT\s+)?DISTINCT\s+FROM\b", re.IGNORECASE)
# `EXTRACT(epoch FROM expires_at)` and `SUBSTRING(x FROM y)`: FROM separates
# call arguments, and what follows is a column.
FROM_IN_CALL = re.compile(
    r"\b(?:EXTRACT|SUBSTRING|TRIM|OVERLAY|POSITION)\s*\(\s*[\w\s']*?\bFROM\b",
    re.IGNORECASE,
)
# Single-quoted SQL literals carry user-facing prose ('... km from your city').
STRING_LITERAL = re.compile(r"'(?:[^']|'')*'", re.DOTALL)
# Any `name AS (` binds a name usable as a relation later in the statement.
# Anchoring on WITH or a preceding comma missed chained and nested CTEs, and
# being slightly permissive here only ever suppresses a warning about a name
# the query itself defines.
# The optional `(col, col)` covers the column-list form, `WITH task(item_key)
# AS (VALUES ...)`, which is how several queries here declare a literal table.
CTE = re.compile(
    r"\"?([a-z_][a-z0-9_]*)\"?\s*(?:\([^()]*\))?\s+AS\s*"
    r"(?:NOT\s+MATERIALIZED\s*|MATERIALIZED\s*)?\(",
    re.IGNORECASE,
)
LINE_COMMENT = re.compile(r"--[^\n]*")
BLOCK_COMMENT = re.compile(r"/\*.*?\*/", re.DOTALL)
# `FOR UPDATE OF event SKIP LOCKED` names a lock target, not a relation to
# scan, and its UPDATE would otherwise read as a write against a table `of`.
LOCK_CLAUSE = re.compile(
    r"\bFOR\s+(?:NO\s+KEY\s+)?(?:UPDATE|SHARE|KEY\s+SHARE)\b"
    r"(?:\s+OF\s+[\w\s,\"]+?)?(?:\s+(?:NOWAIT|SKIP\s+LOCKED))?",
    re.IGNORECASE,
)
# Raw string literals are where the multi-line SQL lives.
RAW_STRING = re.compile(r'r#"(.*?)"#', re.DOTALL)
STATEMENT = re.compile(r"\b(?:SELECT|INSERT\s+INTO|UPDATE|DELETE\s+FROM|WITH)\b", re.IGNORECASE)

# Set-returning functions and syntax that follow FROM but name no relation.
NOT_RELATIONS = {
    # Set-returning functions.
    "unnest", "generate_series", "jsonb_array_elements", "json_array_elements",
    "jsonb_array_elements_text", "jsonb_to_recordset", "jsonb_each", "regexp_split_to_table",
    "string_to_table", "generate_subscripts", "jsonb_each_text", "pg_notify", "pg_sleep",
    # Keywords that can follow one of the scanned verbs.
    "lateral", "rows", "values", "select", "set", "only", "skip", "recursive",
    # `excluded` is the pseudo-relation bound by ON CONFLICT DO UPDATE.
    "excluded",
}
# Catalogs and other schemas the migrations do not create.
EXTERNAL_PREFIXES = ("pg_", "information_schema")

# Relations owned by another service, so no migration here creates them.
# Each entry is a deliberate cross-service read; adding one is a decision about
# service boundaries, not a way to silence this check.
FOREIGN_RELATIONS = {
    # Owned by the TypeScript agent service. The brain reads it to decide when
    # to dispatch and never writes to it; the executor writes via action
    # dispatch. Documented at the call site in operations/growth_intelligence.rs.
    "agent_service_tasks",
}


def known_relations() -> set[str]:
    names: set[str] = set()
    for path in sorted(MIGRATIONS.glob("[0-9][0-9][0-9][0-9]_*.sql")):
        text = path.read_text(encoding="utf-8")
        names.update(name.lower() for name in CREATE.findall(text))
        for old, new in RENAME.findall(text):
            names.discard(old.lower())
            names.add(new.lower())
    return names


def sql_literals() -> list[tuple[Path, str]]:
    found: list[tuple[Path, str]] = []
    for path in sorted(CRATES.rglob("*.rs")):
        if "target" in path.parts:
            continue
        for literal in RAW_STRING.findall(path.read_text(encoding="utf-8", errors="ignore")):
            if not STATEMENT.search(literal):
                continue
            # SQL comments carry English prose, and prose contains the words
            # `from` and `of the`, which the reference scan would read as
            # relation names. Strip them before scanning.
            stripped = literal
            for pattern in (BLOCK_COMMENT, LINE_COMMENT, STRING_LITERAL, LOCK_CLAUSE, DISTINCT_FROM, FROM_IN_CALL):
                stripped = pattern.sub(" ", stripped)
            found.append((path, stripped))
    return found


def unknown_references(literal: str, relations: set[str]) -> set[str]:
    local = {name.lower() for name in CTE.findall(literal)}
    unknown: set[str] = set()
    for match in REFERENCE.finditer(literal):
        lowered = match.group(1).lower()
        if lowered in relations or lowered in local or lowered in NOT_RELATIONS:
            continue
        if lowered in FOREIGN_RELATIONS:
            continue
        if lowered.startswith(EXTERNAL_PREFIXES):
            continue
        # `EXTRACT(day FROM now())`, `IS DISTINCT FROM ROW(...)`: a call, not a
        # relation. Checked on the text after the match so the identifier
        # itself is never shortened to satisfy a lookahead.
        if literal[match.end() :].lstrip().startswith("("):
            continue
        unknown.add(lowered)
    return unknown


class SqlIdentifiersV1(unittest.TestCase):
    def test_migrations_declare_relations(self) -> None:
        self.assertGreater(len(known_relations()), 100, "migration parse produced too few tables")

    def test_sql_literals_were_found(self) -> None:
        self.assertGreater(len(sql_literals()), 100, "raw-string SQL scan found too little")

    def test_every_referenced_relation_exists(self) -> None:
        relations = known_relations()
        problems: list[str] = []
        for path, literal in sql_literals():
            for name in sorted(unknown_references(literal, relations)):
                rel = path.relative_to(ROOT).as_posix()
                problems.append(f"{rel}: no migration creates '{name}'")
        self.assertEqual(sorted(set(problems)), [], "\n".join(sorted(set(problems))))


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    if result.wasSuccessful():
        print(
            f"SQL_IDENTIFIERS_V1=PASS relations={len(known_relations())} "
            f"statements={len(sql_literals())}"
        )
    else:
        print("SQL_IDENTIFIERS_V1=FAIL")
        sys.exit(1)
