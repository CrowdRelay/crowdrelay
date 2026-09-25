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
type of every bind it can, leaves the rest `unknown`, and prepares the
statement both ways. It infers from:

- the expression itself: `.into_uuid()`, `i64::from`, `as i32`, a string
  literal, `format!`, `json!`, `OffsetDateTime::now_utc()`, and `Some(..)`,
  `.clone()`, `.as_deref()` and the like around any of these;
- a typed constant, in the file or uniquely named in the workspace;
- an identifier declared in the enclosing `fn` signature or by a `let`, typed
  or with an initialiser inferable by the rules above;
- a struct field, `input.workspace_id` or `self.limit`, read from the struct
  the enclosing function names, else from the field name when every struct in
  the workspace agrees on its type.

In September 2026 this typed 49% of the 6,662 bound parameters; the field,
`let` and constant rules brought it to 80%. The floor in the test below keeps
it there. A statement that prepares untyped but not typed is the
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
    "[uuid::Uuid]": "uuid[]", "Vec<uuid::Uuid>": "uuid[]",
    "[u8]": "bytea", "Vec<u8>": "bytea",
}


def pg_type(rust: str) -> str | None:
    # Lifetimes go before whitespace does: `&'static str` must not collapse
    # into `'staticstr` and then lose the `str` along with the lifetime.
    rust = re.sub(r"'\w+\s*", "", rust)
    rust = re.sub(r"\s+", "", rust).lstrip("&")
    option = re.fullmatch(r"Option<(.*)>", rust)
    if option:
        rust = option.group(1).lstrip("&")
    return RUST_TO_PG.get(rust)


# A declaration, closed by a brace at the indentation it opened at, so a
# struct declared inside a function does not run on into the code after it.
STRUCT = re.compile(
    r"^([ \t]*)(?:pub(?:\([\w:]+\))?\s+)?struct\s+(\w+)\s*(?:<[^>{]*>)?\s*\{\n(.*?)\n\1\}",
    re.S | re.M,
)
FIELD = re.compile(r"^\s*(?:pub(?:\([\w:]+\))?\s+)?(\w+)\s*:\s*([^,\n]+?),?\s*$", re.M)


class Fields:
    """Struct field types, for binds like `input.workspace_id`.

    When the enclosing function names the binding's struct (`input:
    &CreateThing`, `let row: ThingRow = ...`, or `self` inside `impl Thing`),
    the field is read from that struct. Otherwise a field is only typed when
    every struct field of that name in the workspace translates to the same
    PostgreSQL type. Any disagreement, or a name declared by two structs that
    disagree, stays `unknown`: a wrong type here would report a statement broken
    that is not, or pass one that is.
    """

    def __init__(self, sources: list[str]) -> None:
        by_struct: dict[str, dict[str, set[str | None]]] = {}
        by_name: dict[str, set[str | None]] = {}
        for text in sources:
            for _, struct, body in STRUCT.findall(text):
                fields = by_struct.setdefault(struct, {})
                for name, rust in FIELD.findall(body):
                    pg = pg_type(rust)
                    fields.setdefault(name, set()).add(pg)
                    by_name.setdefault(name, set()).add(pg)
        self.by_struct = {
            struct: {name: only(types) for name, types in fields.items()}
            for struct, fields in by_struct.items()
        }
        self.unique = {name: pg for name, types in by_name.items() if (pg := only(types))}

    def lookup(self, struct: str | None, field: str) -> str:
        if struct and struct in self.by_struct:
            return self.by_struct[struct].get(field) or "unknown"
        return self.unique.get(field, "unknown")


def only(types: set[str | None]) -> str | None:
    return next(iter(types)) if len(types) == 1 else None


def struct_name(rust: str) -> str | None:
    """`&'a mut Thing<T>` -> `Thing`; anything else that is not a bare path, None."""
    rust = re.sub(r"'\w+\s*", "", rust)
    rust = re.sub(r"\s+", "", rust).lstrip("&")
    rust = re.sub(r"^mut", "", rust) if rust.startswith("mut") and rust[3:4].isupper() else rust
    match = re.fullmatch(r"(?:\w+::)*([A-Z]\w*)(?:<.*>)?", rust)
    return match.group(1) if match else None


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


def balanced(expression: str) -> bool:
    depth = 0
    for character in expression:
        depth += {"(": 1, "[": 1, "{": 1, ")": -1, "]": -1, "}": -1}.get(character, 0)
        if depth < 0:
            return False
    return depth == 0


IMPL = re.compile(r"^[ \t]*impl\b[^{;]*?(?:\bfor\s+)?(?:\w+::)*([A-Z]\w*)\s*(?:<[^{]*>)?\s*(?:where[^{]*)?\{", re.M)


def declared_types(
    text: str, position: int, constants: dict[str, str], fields: Fields
) -> dict[str, str]:
    """Identifier -> PostgreSQL type, and `@identifier` -> struct name."""
    signatures = list(FN_SIGNATURE.finditer(text, 0, position))
    if not signatures:
        return {}
    signature = signatures[-1]
    found = {}
    impls = list(IMPL.finditer(text, 0, signature.start()))
    if impls and re.search(r"\bself\b", signature.group(1)):
        found["@self"] = impls[-1].group(1)
    for name, rust in re.findall(r"(\w+)\s*:\s*([^,]+?)(?:,|$)", signature.group(1)):
        if pg := pg_type(rust):
            found[name] = pg
        elif struct := struct_name(rust):
            found["@" + name] = struct
    body = text[signature.end() : position]
    for name, rust, expression in re.findall(
        r"let\s+(?:mut\s+)?(\w+)\s*(?::\s*([^=;]+?)\s*)?=\s*([^;]{1,300});", body
    ):
        found.pop("@" + name, None)
        if rust:
            pg = pg_type(rust)
            if not pg and (struct := struct_name(rust)):
                found["@" + name] = struct
        elif balanced(expression):
            pg = infer(expression, constants, found, fields)
        else:
            # The `;` ended the match inside brackets (`vec![x; n]`), so the
            # expression is a fragment and its tail says nothing.
            pg = None
        # A later `let` shadows the earlier one even when its type is unknown.
        if pg and pg != "unknown":
            found[name] = pg
        else:
            found.pop(name, None)
    return found


PASS_THROUGH = re.compile(
    r"^(.*)\.(?:clone|as_deref|as_ref|to_owned|as_slice|copied|cloned|as_uuid)\(\)$", re.S
)


def infer(
    argument: str,
    constants: dict[str, str],
    declared: dict[str, str],
    fields: Fields | None = None,
) -> str:
    argument = argument.strip().lstrip("&").strip()
    if argument.endswith(".into_uuid()") or argument in ("Uuid::now_v7()", "Uuid::new_v4()", "Uuid::nil()"):
        return "uuid"
    if re.search(r"\.map\((?:\|\w+\|\s*\w+\.into_uuid\(\)|\w+::into_uuid|Into::<Uuid>::into)\)$", argument):
        return "uuid"
    if argument in ("OffsetDateTime::now_utc()", "time::OffsetDateTime::now_utc()"):
        return "timestamp with time zone"
    if argument.endswith(".unix_timestamp()"):
        return "bigint"
    if re.match(r"(?:serde_json::)?json!\s*\(|(?:sqlx::types::)?Json\(", argument):
        return "jsonb"
    if re.match(r"format!\s*\(", argument):
        return "text"
    if (inner := re.fullmatch(r"Some\((.*)\)", argument, re.S)) or (inner := PASS_THROUGH.match(argument)):
        return infer(inner.group(1), constants, declared, fields)
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
    if argument in declared:
        return declared[argument]
    if fields and re.fullmatch(r"[a-z_]\w*(?:\.[a-z_]\w*)+", argument):
        path = argument.split(".")
        # Only `binding.field` has a known struct; deeper paths fall back to the name.
        struct = declared.get("@" + path[0]) if len(path) == 2 else None
        return fields.lookup(struct, path[-1])
    return "unknown"


def shared_constants(sources: list[str]) -> dict[str, str]:
    """Constants imported from another module, typed when the name is unique."""
    seen: dict[str, set[str | None]] = {}
    for text in sources:
        for name, rust in TYPED_CONST.findall(text):
            seen.setdefault(name, set()).add(pg_type(rust))
    return {
        name: next(iter(types))
        for name, types in seen.items()
        if len(types) == 1 and None not in types
    }


def sources() -> list[tuple[str, str]]:
    found = []
    for path in sorted((ROOT / "crates").rglob("*.rs")):
        relative = path.relative_to(ROOT).as_posix()
        if not _types.is_test_source(relative):
            found.append((relative, path.read_text(errors="ignore")))
    return found


def statements() -> list[tuple[str, int, str, list[str]]]:
    found = []
    files = sources()
    fields = Fields([text for _, text in files])
    shared = shared_constants([text for _, text in files])
    for relative, text in files:
        constants = shared | {
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
            declared = declared_types(text, literal.start(), constants, fields)
            types = [infer(argument, constants, declared, fields) for argument in arguments[:parameters]]
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
        self.assertGreater(typed, 5200, f"only {typed} parameters were typed")
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
