#!/usr/bin/env python3
"""A statement still resolves once its parameters carry the types sqlx sends.

`sql-result-types.py` prepares every query with its parameters untyped, and
PostgreSQL then infers whatever type makes the statement work. sqlx does not
leave them untyped: it sends the Rust type of every bind. So
`make_interval(days => $2)` prepares clean untyped — `$2` is inferred as
integer — and fails on first request with `function make_interval(days =>
bigint) does not exist` when the bind is an `i64`, because PostgreSQL has no
implicit bigint-to-integer cast for function resolution. `date + $n` with an
`i64` fails the same way, as does any operator or function whose only overload
takes the narrower type.

This reads each literal statement's `.bind(...)` chain, infers the PostgreSQL
type of every bind it can (`.into_uuid()`, `i64::from`, `as i32`, a string
literal, a typed constant, or an identifier declared in the enclosing `fn`
signature or a typed `let`), leaves the rest `unknown`, and prepares the
statement both ways. A statement that prepares untyped but not typed is the
failure. Statements that fail untyped are `sql-result-types.py`'s business
and are not judged here.

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

FETCH = re.compile(r"\.(?:execute|fetch_one|fetch_optional|fetch_all|fetch_many|fetch)\b")
FN_SIGNATURE = re.compile(r"fn\s+\w+\s*(?:<[^>]*>)?\s*\((.*?)\)\s*(?:->[^{;]*)?\{", re.S)
TYPED_CONST = re.compile(r"const\s+([A-Z][A-Z0-9_]*)\s*:\s*([\w&\[\]<>:']+)\s*=")

RUST_TO_PG = {
    "Uuid": "uuid", "uuid::Uuid": "uuid",
    "i64": "bigint", "i32": "integer", "i16": "smallint",
    "f64": "double precision", "f32": "real", "bool": "boolean",
    "str": "text", "String": "text",
    "OffsetDateTime": "timestamp with time zone",
    "time::OffsetDateTime": "timestamp with time zone",
    "Date": "date", "time::Date": "date",
    "Value": "jsonb", "serde_json::Value": "jsonb",
    "[String]": "text[]", "Vec<String>": "text[]", "[&str]": "text[]", "Vec<&str>": "text[]",
    "[Uuid]": "uuid[]", "Vec<Uuid>": "uuid[]", "[i64]": "bigint[]", "Vec<i64>": "bigint[]",
}


def pg_type(rust: str) -> str | None:
    rust = re.sub(r"\s+", "", rust).lstrip("&")
    rust = re.sub(r"^'\w+", "", rust)
    option = re.fullmatch(r"Option<(.*)>", rust)
    if option:
        rust = option.group(1).lstrip("&")
    return RUST_TO_PG.get(rust)


def bind_arguments(chain: str) -> list[str]:
    arguments, index = [], 0
    while (start := chain.find(".bind(", index)) >= 0:
        end, depth = start + 6, 1
        while depth and end < len(chain):
            depth += {"(": 1, ")": -1}.get(chain[end], 0)
            end += 1
        arguments.append(chain[start + 6 : end - 1].strip())
        index = end
    return arguments


def declared_types(text: str, position: int) -> dict[str, str]:
    signatures = list(FN_SIGNATURE.finditer(text, 0, position))
    if not signatures:
        return {}
    signature = signatures[-1]
    found = {}
    for name, rust in re.findall(r"(\w+)\s*:\s*([^,]+?)(?:,|$)", signature.group(1)):
        if pg := pg_type(rust):
            found[name] = pg
    for name, rust in re.findall(r"let\s+(?:mut\s+)?(\w+)\s*:\s*([^=;]+?)\s*=", text[signature.end() : position]):
        if pg := pg_type(rust):
            found[name] = pg
    return found


def infer(argument: str, constants: dict[str, str], declared: dict[str, str]) -> str:
    argument = argument.strip().lstrip("&")
    if argument.endswith(".into_uuid()") or argument in ("Uuid::now_v7()", "Uuid::new_v4()"):
        return "uuid"
    for rust, pg in (("i64", "bigint"), ("i32", "integer"), ("i16", "smallint")):
        if re.search(rf"^{rust}::(?:from|try_from)\(|\bas {rust}$", argument):
            return pg
    if re.search(r"^f64::from\(|\bas f64$", argument):
        return "double precision"
    if re.fullmatch(r'"(?:[^"\\]|\\.)*"', argument) or re.search(r"\.(?:as_str|to_string|trim)\(\)$", argument):
        return "text"
    if argument in ("true", "false"):
        return "boolean"
    if argument in constants:
        return constants[argument]
    return declared.get(argument, "unknown")


def statements() -> list[tuple[str, int, str, list[str]]]:
    found = []
    for path in sorted((ROOT / "crates").rglob("*.rs")):
        relative = path.relative_to(ROOT).as_posix()
        if _types.is_test_source(relative):
            continue
        text = path.read_text(errors="ignore")
        constants = {
            name: pg for name, rust in TYPED_CONST.findall(text) if (pg := pg_type(rust))
        }
        for literal in _types.RAW_LITERAL.finditer(text):
            sql = literal.group(1).strip().rstrip(";")
            if re.search(r"\{[a-z_0-9]*\}", sql) or _types.names_a_foreign_relation(sql):
                continue  # A format! template, or a table this database does not own.
            if ";" in re.sub(r"'[^']*'", "", sql):
                continue  # Multi-statement; PREPARE takes one.
            tail = text[literal.end() : literal.end() + 5000]
            fetch = FETCH.search(tail)
            if not fetch or "sqlx::query" in tail[: fetch.start()]:
                continue  # No chain, or the chain ran into the next query.
            arguments = bind_arguments(tail[: fetch.start()])
            parameters = max((int(n) for n in re.findall(r"\$(\d+)", sql)), default=0)
            if parameters == 0 or len(arguments) < parameters:
                continue
            declared = declared_types(text, literal.start())
            types = [infer(argument, constants, declared) for argument in arguments[:parameters]]
            if all(pg == "unknown" for pg in types):
                continue
            line = text[: literal.start()].count("\n") + 1
            found.append((relative, line, sql, types))
    return found


def failures(container: str, items: list, typed: bool) -> dict[int, str]:
    script = ["\\set ON_ERROR_STOP 0", "\\set QUIET 1"]
    for index, (_, _, sql, types) in enumerate(items):
        signature = ",".join(types if typed else ["unknown"] * len(types))
        script.append(f"PREPARE typed_{index}({signature}) AS {sql};")
        script.append(f"\\if :ERROR\n\\echo FAIL_{index} :LAST_ERROR_MESSAGE\n\\endif")
    script.append("DEALLOCATE ALL;")
    result = _types.psql("\n".join(script), container)
    found = {}
    for line in result.stdout.splitlines():
        if line.startswith("FAIL_"):
            marker, _, message = line.partition(" ")
            found[int(marker[5:])] = message
    return found


class SqlTypedParameters(unittest.TestCase):
    def setUp(self) -> None:
        self.container = _types.find_container()
        if not self.container:
            self.skipTest("no local crowdrelay-postgres-1 container; run `just db-up`")

    def test_bind_types_do_not_break_resolution(self) -> None:
        items = statements()
        typed = sum(1 for *_, types in items for pg in types if pg != "unknown")
        self.assertGreater(typed, 2500, f"only {typed} parameters were typed")
        untyped = failures(self.container, items, typed=False)
        with_types = failures(self.container, items, typed=True)
        self.assertLess(len(untyped), len(items) // 5, "untyped PREPARE mostly failed; wrong database?")
        broken = [
            f"{items[i][0]}:{items[i][1]} {message}  types={items[i][3]}"
            for i, message in sorted(with_types.items())
            if i not in untyped
        ]
        self.assertEqual(
            broken,
            [],
            "these statements resolve untyped but not with the types sqlx "
            "sends:\n  " + "\n  ".join(broken),
        )


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    print("SQL_TYPED_PARAMS=" + ("PASS" if result.wasSuccessful() else "FAIL"))
    sys.exit(0 if result.wasSuccessful() else 1)
