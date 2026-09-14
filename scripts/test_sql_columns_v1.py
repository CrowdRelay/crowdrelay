#!/usr/bin/env python3
"""Check that runtime SQL only names columns the migrations actually create.

`test_sql_identifiers_v1.py` recovered half of what the compile-time SQLx
macros would have given: every relation named after FROM/JOIN/INSERT INTO/
UPDATE/DELETE FROM exists. It says so itself — "It does not typecheck columns
— `just test-postgres` does that against a real schema." But `test-postgres`
only reaches columns on code paths a test drives, and most are not driven.

The gap is not theoretical. `operations/show_growth_execution.rs` shipped with

    LEFT JOIN cities AS city
      ON city.workspace_id = event.workspace_id

`cities` is a shared catalogue with no `workspace_id`, so the statement did
not parse and `execute_show_growth` could never run. It compiled, passed
clippy, passed the whole test suite and passed every contract script. In
production every `show.growth.request` action failed `unexpected` — four of
them, five attempts each, zero successes, for the entire life of the feature.

This closes that half. Every **qualified** column reference (`alias.column`)
whose alias is unambiguously bound to one real table must name a column some
migration creates on that table.

Deliberately conservative, because a false positive here is a broken build for
correct code. A reference is skipped whenever the binding is not certain:

  * the alias binds to a CTE, a subquery, a LATERAL, or a set-returning call
  * the same alias binds to two different tables in one statement (UNION
    branches reuse `target` for two tables in `growth_debt.rs`)
  * the relation is a view, whose columns these migrations do not spell out
  * the relation is foreign, per `test_sql_identifiers_v1.FOREIGN_RELATIONS`

Unqualified columns are not checked at all. Resolving them needs to know which
table in a join owns each name, and guessing wrong is exactly the failure this
script exists to prevent.
"""
from __future__ import annotations

import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MIGRATIONS = ROOT / "migrations"
CRATES = ROOT / "crates"

LINE_COMMENT = re.compile(r"--[^\n]*")
BLOCK_COMMENT = re.compile(r"/\*.*?\*/", re.DOTALL)
STRING_LITERAL = re.compile(r"'(?:[^']|'')*'", re.DOTALL)
RAW_STRING = re.compile(r'r#"(.*?)"#', re.DOTALL)
# Plain literals carry SQL too, and this gate could not see them until
# 2026-09-14 — the same blind spot `sql-result-types.py` and
# `test_sql_identifiers_v1.py` had, in the third of the three checks standing in
# for compile-time SQL verification.
PLAIN_STRING = re.compile(r'"((?:[^"\\]|\\.)*)"', re.DOTALL)
# A plain literal must *begin* with a statement keyword and carry a structural
# token. `STATEMENT` matches its keywords anywhere, which is right for a raw
# literal and wrong for English: without both tests the reference scan reads the
# nouns in an assertion message as relations and columns. A raw literal is SQL by
# convention and needs neither.
PLAIN_SQL_START = re.compile(
    r"^\s*(?:select|insert\s+into|update|delete\s+from|with)\b", re.IGNORECASE
)
SQL_SHAPE = re.compile(r"\s(?:from|into|set)\s|::", re.IGNORECASE)
STATEMENT = re.compile(
    r"\b(?:SELECT|INSERT\s+INTO|UPDATE|DELETE\s+FROM|WITH)\b", re.IGNORECASE
)

CREATE_TABLE = re.compile(
    r"CREATE\s+(?:UNLOGGED\s+|TEMP(?:ORARY)?\s+)?TABLE\s+"
    r"(?:IF\s+NOT\s+EXISTS\s+)?(?:public\.)?\"?(\w+)\"?\s*\(",
    re.IGNORECASE,
)
CREATE_VIEW = re.compile(
    r"CREATE\s+(?:OR\s+REPLACE\s+)?(?:MATERIALIZED\s+)?VIEW\s+"
    r"(?:IF\s+NOT\s+EXISTS\s+)?(?:public\.)?\"?(\w+)\"?",
    re.IGNORECASE,
)
ALTER_TABLE = re.compile(
    r"ALTER\s+TABLE\s+(?:IF\s+EXISTS\s+)?(?:ONLY\s+)?(?:public\.)?\"?(\w+)\"?"
    r"(.*?);",
    re.IGNORECASE | re.DOTALL,
)
ADD_COLUMN = re.compile(
    r"ADD\s+COLUMN\s+(?:IF\s+NOT\s+EXISTS\s+)?\"?(\w+)\"?", re.IGNORECASE
)
DROP_COLUMN = re.compile(
    r"DROP\s+COLUMN\s+(?:IF\s+EXISTS\s+)?\"?(\w+)\"?", re.IGNORECASE
)
RENAME_COLUMN = re.compile(
    r"RENAME\s+COLUMN\s+\"?(\w+)\"?\s+TO\s+\"?(\w+)\"?", re.IGNORECASE
)
RENAME_TABLE = re.compile(r"RENAME\s+TO\s+\"?(\w+)\"?", re.IGNORECASE)
DROP_TABLE = re.compile(
    r"DROP\s+TABLE\s+(?:IF\s+EXISTS\s+)?(?:public\.)?\"?(\w+)\"?", re.IGNORECASE
)

# A column definition never starts with one of these; a table constraint does.
CONSTRAINT_LEADS = {
    "constraint", "primary", "foreign", "unique", "check", "exclude", "like",
}

# `alias.column` — the only form this script judges.
QUALIFIED = re.compile(r"\b([a-z_][a-z0-9_]*)\.([a-z_][a-z0-9_]*)\b")
# `FROM tbl alias`, `JOIN tbl AS alias`, `UPDATE tbl alias`.
ALIASED = re.compile(
    r"\b(?:FROM|JOIN|UPDATE)\s+(?:ONLY\s+)?(?:public\.)?\"?([a-z_][a-z0-9_]*)\"?"
    r"\s+(?:AS\s+)?\"?([a-z_][a-z0-9_]*)\"?\b",
    re.IGNORECASE,
)
# A relation named with no alias binds its own name as the qualifier.
BARE = re.compile(
    r"\b(?:FROM|JOIN|UPDATE|INSERT\s+INTO|DELETE\s+FROM)\s+(?:ONLY\s+)?(?:public\.)?"
    r"\"?([a-z_][a-z0-9_]*)\"?",
    re.IGNORECASE,
)
# `... ) alias` and `... ) AS alias` bind a subquery or LATERAL, whose columns
# are computed rather than declared.
DERIVED = re.compile(r"\)\s*(?:AS\s+)?\"?([a-z_][a-z0-9_]*)\"?", re.IGNORECASE)
CTE = re.compile(
    r"\"?([a-z_][a-z0-9_]*)\"?\s*(?:\([^()]*\))?\s+AS\s*"
    r"(?:NOT\s+MATERIALIZED\s*|MATERIALIZED\s*)?\(",
    re.IGNORECASE,
)

# Words that follow a scanned verb without naming a relation, plus the
# pseudo-relations SQL binds for us.
NOT_RELATIONS = {
    "lateral", "rows", "values", "select", "set", "only", "skip", "recursive",
    "excluded", "unnest", "generate_series", "jsonb_array_elements",
    "json_array_elements", "jsonb_array_elements_text", "jsonb_to_recordset",
    "jsonb_each", "jsonb_each_text", "regexp_split_to_table", "string_to_table",
    "generate_subscripts", "pg_notify", "pg_sleep",
}

# Reserved words that can appear on the left of a dot in expressions we do not
# want to read as an alias, and type names in casts like `x::public.foo`.
NOT_ALIASES = {"public", "pg_catalog", "information_schema", "excluded"}


def build_schema() -> tuple[dict[str, set[str]], set[str]]:
    """Return `(table -> columns, relations whose columns are unknown)`.

    Views land in the second set: the migrations define them by query, not by
    column list, so nothing here can say which columns they expose.
    """
    tables: dict[str, set[str]] = {}
    opaque: set[str] = set()

    for path in sorted(MIGRATIONS.glob("[0-9][0-9][0-9][0-9]_*.sql")):
        text = path.read_text(encoding="utf-8")
        text = BLOCK_COMMENT.sub(" ", text)
        text = LINE_COMMENT.sub(" ", text)

        for match in CREATE_TABLE.finditer(text):
            name = match.group(1).lower()
            body = balanced_body(text, match.end() - 1)
            if body is None:
                opaque.add(name)
                continue
            tables.setdefault(name, set()).update(column_names(body))

        for match in CREATE_VIEW.finditer(text):
            opaque.add(match.group(1).lower())

        for match in ALTER_TABLE.finditer(text):
            name = match.group(1).lower()
            clause = match.group(2)
            columns = tables.setdefault(name, set())
            columns.update(c.lower() for c in ADD_COLUMN.findall(clause))
            for dropped in DROP_COLUMN.findall(clause):
                columns.discard(dropped.lower())
            for old, new in RENAME_COLUMN.findall(clause):
                columns.discard(old.lower())
                columns.add(new.lower())
            # A table rename carries its columns to the new name. Checked
            # after the column edits so a statement doing both still lands.
            if not RENAME_COLUMN.search(clause):
                renamed = RENAME_TABLE.search(clause)
                if renamed:
                    tables[renamed.group(1).lower()] = columns
                    tables.pop(name, None)

        for dropped in DROP_TABLE.findall(text):
            tables.pop(dropped.lower(), None)
            opaque.discard(dropped.lower())

    return tables, opaque


def balanced_body(text: str, open_paren: int) -> str | None:
    """The text inside the parenthesis at `open_paren`, respecting nesting."""
    depth = 0
    for index in range(open_paren, len(text)):
        char = text[index]
        if char == "(":
            depth += 1
        elif char == ")":
            depth -= 1
            if depth == 0:
                return text[open_paren + 1 : index]
    return None


def column_names(body: str) -> set[str]:
    """First identifier of each top-level item that is not a table constraint."""
    names: set[str] = set()
    depth = 0
    current: list[str] = []
    items: list[str] = []
    in_string = False
    for char in body:
        if char == "'":
            in_string = not in_string
        if not in_string:
            if char == "(":
                depth += 1
            elif char == ")":
                depth -= 1
            elif char == "," and depth == 0:
                items.append("".join(current))
                current = []
                continue
        current.append(char)
    items.append("".join(current))

    for item in items:
        token = item.strip().lstrip('"').split()
        if not token:
            continue
        first = token[0].strip('"').lower()
        if first in CONSTRAINT_LEADS or not re.fullmatch(r"[a-z_][a-z0-9_]*", first):
            continue
        names.add(first)
    return names


def sql_literals() -> list[tuple[Path, str]]:
    found: list[tuple[Path, str]] = []
    for path in sorted(CRATES.rglob("*.rs")):
        if "target" in path.parts:
            continue
        source = path.read_text(encoding="utf-8", errors="ignore")
        # Raw literals first, then blanked out: `PLAIN_STRING` also matches a raw
        # literal's body, since `r#"SELECT ..."#` contains a quote, the query and
        # another quote, so scanning both over the same text reads every raw
        # statement twice.
        raw = RAW_STRING.findall(source)
        plain = [
            literal
            for literal in PLAIN_STRING.findall(RAW_STRING.sub("", source))
            if PLAIN_SQL_START.match(literal) and SQL_SHAPE.search(literal)
        ]
        for literal in raw + plain:
            if not STATEMENT.search(literal):
                continue
            stripped = literal
            for pattern in (BLOCK_COMMENT, LINE_COMMENT, STRING_LITERAL):
                stripped = pattern.sub(" ", stripped)
            found.append((path, stripped))
    return found


def certain_bindings(
    statement: str, tables: dict[str, set[str]], opaque: set[str]
) -> dict[str, str]:
    """Aliases this script is willing to judge, mapped to their table.

    Anything ambiguous is dropped rather than guessed: a name bound twice, a
    CTE, a derived table, or a relation whose columns are unknown.
    """
    local = {name.lower() for name in CTE.findall(statement)}
    local.update(name.lower() for name in DERIVED.findall(statement))

    bindings: dict[str, str] = {}
    ambiguous: set[str] = set()
    for table, alias in ALIASED.findall(statement):
        table_l, alias_l = table.lower(), alias.lower()
        if alias_l in local or table_l in NOT_RELATIONS:
            continue
        if table_l in local:
            # `FROM inserted AS event` in `reminders.rs`, where the same
            # statement also has `FROM events AS event` inside a different
            # CTE. The alias means two different row shapes in two scopes,
            # and this script does not track scopes.
            ambiguous.add(alias_l)
            continue
        if table_l not in tables:
            ambiguous.add(alias_l)
            continue
        if bindings.get(alias_l, table_l) != table_l:
            ambiguous.add(alias_l)
        bindings[alias_l] = table_l

    for table in BARE.findall(statement):
        table_l = table.lower()
        if table_l in local or table_l in NOT_RELATIONS or table_l not in tables:
            continue
        if bindings.get(table_l, table_l) != table_l:
            ambiguous.add(table_l)
        bindings.setdefault(table_l, table_l)

    for name in local | ambiguous | opaque | NOT_ALIASES:
        bindings.pop(name, None)
    return bindings


def unknown_columns(
    statement: str, tables: dict[str, set[str]], opaque: set[str]
) -> set[tuple[str, str]]:
    bindings = certain_bindings(statement, tables, opaque)
    problems: set[tuple[str, str]] = set()
    for alias, column in QUALIFIED.findall(statement):
        table = bindings.get(alias.lower())
        if table is None:
            continue
        if column.lower() not in tables[table]:
            problems.add((table, column.lower()))
    return problems


class SqlColumnsV1(unittest.TestCase):
    def test_the_migration_parse_is_plausible(self) -> None:
        tables, _ = build_schema()
        self.assertGreater(len(tables), 200, "migration parse produced too few tables")
        # Spot-check the shapes this script exists to reason about: a
        # workspace-scoped table, a shared catalogue that is deliberately not
        # scoped, and a column added by a later ALTER rather than by CREATE.
        self.assertIn("workspace_id", tables["events"])
        self.assertNotIn(
            "workspace_id",
            tables["cities"],
            "`cities` is a shared catalogue; if it gained a workspace column "
            "this script's motivating example is obsolete",
        )
        self.assertIn("membership_state", tables["discovery_places"])

    def test_sql_literals_were_found(self) -> None:
        self.assertGreater(len(sql_literals()), 100, "raw-string SQL scan found too little")

    def test_every_qualified_column_exists(self) -> None:
        tables, opaque = build_schema()
        problems: list[str] = []
        for path, statement in sql_literals():
            for table, column in sorted(unknown_columns(statement, tables, opaque)):
                rel = path.relative_to(ROOT).as_posix()
                problems.append(f"{rel}: no migration creates '{table}.{column}'")
        self.assertEqual(sorted(set(problems)), [], "\n".join(sorted(set(problems))))


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    if result.wasSuccessful():
        tables, opaque = build_schema()
        columns = sum(len(cols) for cols in tables.values())
        print(
            f"SQL_COLUMNS_V1=PASS tables={len(tables)} columns={columns} "
            f"views={len(opaque)} statements={len(sql_literals())}"
        )
    else:
        print("SQL_COLUMNS_V1=FAIL")
        sys.exit(1)
