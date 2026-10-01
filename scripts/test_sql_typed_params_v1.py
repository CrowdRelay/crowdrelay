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
    "i64": "bigint", "i32": "integer", "i16": "smallint", "u32": "integer", "u16": "smallint",
    "f64": "double precision", "f32": "real", "bool": "boolean",
    "str": "text", "String": "text",
    "OffsetDateTime": "timestamp with time zone",
    "time::OffsetDateTime": "timestamp with time zone",
    "Date": "date", "time::Date": "date",
    "Value": "jsonb", "serde_json::Value": "jsonb",
    "[String]": "text[]", "Vec<String>": "text[]", "[&str]": "text[]", "Vec<&str>": "text[]",
    "[Uuid]": "uuid[]", "Vec<Uuid>": "uuid[]", "[uuid::Uuid]": "uuid[]", "Vec<uuid::Uuid>": "uuid[]",
    "[i64]": "bigint[]", "Vec<i64>": "bigint[]",
    "[i32]": "integer[]", "Vec<i32>": "integer[]",
    "Vec<u8>": "bytea", "&[u8]": "bytea", "[u8]": "bytea",
    "Duration": "interval", "time::Duration": "interval",
}


def _generic_args(rust: str) -> list[str]:
    """`A<B, C>` → [`B`, `C`], splitting on top-level commas only."""
    open_at = rust.find("<")
    if open_at < 0:
        return []
    close = _balanced(rust, open_at, "<>")
    return _top_level_commas(rust[open_at + 1 : close - 1])


def strip_wrappers(rust: str) -> str:
    """Remove the wrappers a member chain or a `for` element ignores:
    references, lifetimes, Option/Box/Arc, extractor shells (Json/Form/Path/
    Query), and Result — whose Ok payload is the only part a `?`-bind sees."""
    rust = rust.strip().lstrip("&")
    rust = re.sub(r"^'\w+", "", rust).strip()
    if m := re.fullmatch(r"(Option|Box|Arc|Json|Form|Path|Query|Result)<(.*)>", rust, re.S):
        inner = _generic_args(rust)
        return strip_wrappers(inner[0]) if inner else rust
    return re.sub(r"\s+", "", rust)


def pg_type(rust: str) -> str | None:
    rust = strip_wrappers(rust)
    if re.fullmatch(r"\[\s*u8\s*;\s*\d+\s*\]", rust):
        return "bytea"
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


# ---------------------------------------------------------------------------
# Workspace type index. Binds are `command.x`, `row.0`, `preview.totals.gross`
# far more often than bare literals, so the scan collects every struct's field
# types (named and tuple), the `typed_uuid_id!` newtype names, free-function
# return types and per-impl method return types once, workspace-wide.
# ---------------------------------------------------------------------------
_WS: dict | None = None


def _balanced(text: str, open_at: int, pair: str = "()") -> int:
    depth, i = 0, open_at
    while i < len(text):
        depth += {pair[0]: 1, pair[1]: -1}.get(text[i], 0)
        i += 1
        if depth == 0:
            return i
    return len(text)


def workspace_index() -> dict:
    global _WS
    if _WS is not None:
        return _WS
    fields: dict[str, dict[str, str]] = {}
    fields_by_file: dict[str, dict[str, dict[str, str]]] = {}
    tuples: dict[str, dict[int, str]] = {}
    uuid_ids: set[str] = set()
    fn_returns: dict[str, str] = {}
    methods: dict[tuple[str, str], str] = {}
    consts: dict[str, str] = {}
    for path in sorted((ROOT / "crates").rglob("*.rs")):
        rel = path.relative_to(ROOT).as_posix()
        text = path.read_text(errors="ignore")
        for m in re.finditer(
            r"(?:pub\s+)?struct\s+(\w+)\s*(?:<[^{};]*>)?\s*(?:where\s+[^{};]+)?\{", text
        ):
            body = text[m.end() : _balanced(text, m.end() - 1, "{}") - 1]
            body = re.sub(r"//[^\n]*", "", body)
            found = {}
            for fname, fty in re.findall(
                r"(?:pub(?:\([^)]*\))?\s+)?(\w+)\s*:\s*([^,}]+?)\s*[,}]", body + ","
            ):
                found[fname] = fty.strip()
            if found:
                # Struct names repeat across crates (two `ClaimRequest`s); the
                # field sets union so a lookup succeeds on any definition —
                # but a same-file definition always wins (_field_type).
                fields.setdefault(m.group(1), {}).update(found)
                fields_by_file.setdefault(rel, {}).setdefault(m.group(1), {}).update(found)
        for m in re.finditer(
            r"(?:pub\s+)?struct\s+(\w+)\s*(?:<[^{};]*>)?\s*\(([^;{]*)\)", text
        ):
            parts = [p.strip() for p in m.group(2).split(",") if p.strip()]
            tmap = {}
            for idx, part in enumerate(parts):
                tmap[idx] = re.sub(r"^pub(?:\([^)]*\))?\s*", "", part).strip()
            if tmap:
                tuples.setdefault(m.group(1), {}).update(tmap)
        for m in re.finditer(r"typed_uuid_id!\s*\(([^)]*)\)", text):
            for name in re.findall(r"\b([A-Z]\w*)\b", m.group(1)):
                uuid_ids.add(name)
                tuples.setdefault(name, {0: "Uuid"})
        for name, rust in TYPED_CONST.findall(text):
            consts.setdefault(name, rust)
        for m in re.finditer(r"fn\s+(\w+)\s*(?:<[^>]*>)?\s*\(", text):
            close = _balanced(text, m.end() - 1)
            tail = text[close : close + 200]
            if ret := re.match(r"\s*->\s*([^{;]+?)\s*[{;]", tail):
                fn_returns.setdefault(m.group(1), ret.group(1).strip())
        for m in re.finditer(
            r"impl\s*(?:<[^>]*>)?\s+([\w:]+)(?:\s*<[^>]*>)?(?:\s+for\s+([\w:]+))?\s*\{", text
        ):
            self_ty = (m.group(2) or m.group(1)).split("::")[-1]
            body = text[m.end() : _balanced(text, m.end() - 1, "{}") - 1]
            for fm in re.finditer(r"fn\s+(\w+)\s*(?:<[^>]*>)?\s*\(", body):
                close = _balanced(body, fm.end() - 1)
                if ret := re.match(r"\s*->\s*([^{;]+?)\s*[{;]", body[close : close + 200]):
                    methods.setdefault((self_ty, fm.group(1)), ret.group(1).strip())
    _WS = {
        "fields": fields,
        "fields_by_file": fields_by_file,
        "tuples": tuples,
        "uuid_ids": uuid_ids,
        "fn_returns": fn_returns,
        "methods": methods,
        "consts": consts,
    }
    return _WS


# Struct fields defined in the file currently under analysis — same-named
# structs in other files must not shadow these (a `Row`/`Command` collision
# mistyped `expected_version`/`id` binds).
_CUR_FIELDS: dict[str, dict[str, str]] = {}


def pg_resolved(rust: str) -> str | None:
    """`pg_type` plus the workspace's own wrappers: a `typed_uuid_id!` name or
    a single-field tuple struct binds as its inner type."""
    if pg := pg_type(rust):
        return pg
    index = workspace_index()
    name = strip_wrappers(rust).split("::")[-1]
    if name in index["uuid_ids"]:
        return "uuid"
    tmap = index["tuples"].get(name)
    if tmap and len(tmap) == 1:
        return pg_resolved(next(iter(tmap.values())))
    return None


def _top_level_commas(params: str) -> list[str]:
    parts, depth, start = [], 0, 0
    for i, ch in enumerate(params):
        if ch in "<([":
            depth += 1
        elif ch in ">)]":
            depth -= 1
        elif ch == "," and depth == 0:
            parts.append(params[start:i])
            start = i + 1
    parts.append(params[start:])
    return parts


def _self_type(text: str, position: int) -> str | None:
    found = None
    for m in re.finditer(
        r"impl\s*(?:<[^>]*>)?\s+([\w:]+)(?:\s*<[^>]*>)?(?:\s+for\s+([\w:]+))?\s*\{", text[:position]
    ):
        found = m
    if not found:
        return None
    return (found.group(2) or found.group(1)).split("::")[-1]


def _field_type(rust: str, segment: str) -> str | None:
    index = workspace_index()
    name = re.sub(r"<.*>$", "", strip_wrappers(rust).split("::")[-1])
    if name.startswith("(") and name.endswith(")"):
        # An anonymous query_as::<_, (A, B)> row tuple.
        parts = _top_level_commas(name[1:-1])
        if segment.isdigit() and int(segment) < len(parts):
            return parts[int(segment)].strip()
        return None
    if segment.isdigit():
        return index["tuples"].get(name, {}).get(int(segment))
    fmap = _CUR_FIELDS.get(name) or index["fields"].get(name) or {}
    return fmap.get(segment)


def _member_chain(argument: str, declared_rust: dict[str, str]) -> str | None:
    """Resolve `base.seg.seg…`: the base variable's declared Rust type, then
    walk struct fields / tuple indexes segment by segment."""
    m = re.fullmatch(r"(self|\w+)((?:\.\w+)+)", argument, re.S)
    if not m:
        return None
    rust = declared_rust.get(m.group(1))
    for segment in m.group(2)[1:].split("."):
        if rust is None:
            return None
        rust = _field_type(rust, segment)
    return pg_resolved(rust) if rust is not None else None


def _method_chain(argument: str, declared_rust: dict[str, str]) -> str | None:
    """`base.method(…)` or `base.field.method(…)` — the receiver's type plus
    the impl's declared return type."""
    m = re.fullmatch(r"(self|\w+)((?:\.\w+)*)\.(\w+)\(.*\)", argument, re.S)
    if not m:
        return None
    base, segments, method = m.groups()
    rust = declared_rust.get(base)
    if rust is None:
        return None
    for segment in segments[1:].split(".") if segments else ():
        rust = _field_type(rust, segment)
        if rust is None:
            return None
    ret = workspace_index()["methods"].get(
        (strip_wrappers(rust).split("::")[-1], method)
    )
    return pg_resolved(ret) if ret else None


def _let_rhss(body: str):
    """`(name, rhs)` for every `let name = rhs;` — the RHS may itself hold
    `;` inside `vec![x; n]`, block expressions, or `r#"…"#` literals, so a
    plain `[^;]+` truncates it. Scan to the depth-0 `;` instead."""
    for m in re.finditer(r"let\s+(?:mut\s+)?(\w+)\s*=\s*", body):
        start, depth, i = m.end(), 0, m.end()
        while i < len(body):
            ch = body[i]
            if ch in "([{":
                depth += 1
            elif ch in ")]}":
                depth -= 1
            elif ch == ";" and depth == 0:
                break
            i += 1
        yield m.group(1), body[start:i].strip()


def _for_element(name: str, declared_rust: dict[str, str]) -> str | None:
    enumerate_ = name.endswith(".enumerate()")
    for suffix in (".enumerate()", ".iter()", ".iter_mut()", ".into_iter()"):
        if name.endswith(suffix):
            name = name[: -len(suffix)]
            break
    rust = declared_rust.get(name) or _member_rust(name, declared_rust)
    if rust is None:
        return None
    element = _element_rust(rust)
    if element is None:
        return None
    return f"(usize, {element})" if enumerate_ else element


def declared_types(text: str, position: int) -> tuple[dict[str, str], dict[str, str]]:
    """(name → pg, name → rust) visible at `position`: the enclosing `fn`'s
    params, `self` from the enclosing `impl`, typed and unannotated `let`s, and
    `for` loop element variables."""
    pg: dict[str, str] = {}
    rust: dict[str, str] = {}
    signatures = list(
        re.finditer(r"fn\s+\w+\s*(?:<[^>]*>)?\s*\(", text[:position])
    )
    if not signatures:
        return pg, rust
    signature = signatures[-1]
    close = _balanced(text, signature.end() - 1)
    params = text[signature.end() - 1 : close]
    for part in _top_level_commas(params[1:-1]):
        part = part.strip()
        # `Path(player_id): Path<Uuid>` — an extractor destructured in the
        # signature itself; the bound name's type is the wrapper's argument.
        if m := re.match(r"(\w+)\((\w+)\)\s*:\s*(.+)$", part, re.S):
            name, rust_ty = m.group(2), m.group(3).strip()
            generic = _generic_args(rust_ty)
            rust[name] = generic[0].strip() if generic else rust_ty
            if resolved := pg_resolved(rust[name]):
                pg[name] = resolved
            continue
        if m := re.match(r"(?:mut\s+)?(\w+)\s*:\s*(.+)$", part, re.S):
            name, rust_ty = m.group(1), m.group(2).strip()
            rust[name] = rust_ty
            if resolved := pg_resolved(rust_ty):
                pg[name] = resolved
    if self_ty := _self_type(text, signature.start()):
        rust["self"] = self_ty
    body = text[close:position]
    for m in re.finditer(r"let\s+(?:mut\s+)?(\w+)\s*:\s*([^=;]+?)\s*=", body):
        name, rust_ty = m.group(1), m.group(2).strip()
        rust[name] = rust_ty
        if resolved := pg_resolved(rust_ty):
            pg[name] = resolved
    # Local closures: `let bounded = |v: u32| -> Result<i32, E> { … }` — the
    # declared return type is what `bounded(args)` produces.
    for m in re.finditer(r"let\s+(\w+)\s*=\s*\|[^|]*\|\s*->\s*([^\{]+)\{", body):
        rust[m.group(1)] = f"fn:({m.group(2).strip()})"
    for name, rhs in _let_rhss(body):
        if name in pg or name in rust:
            continue
        inferred = infer(rhs, {}, pg, rust)
        resolved = _rust_expr(rhs, rust) or _rust_of(rhs, rust, pg)
        # query_as/scalar's turbofish is the *row* type; the fetch shape
        # wraps it — fetch_all/fetch_many → Vec<T>, fetch_optional →
        # Option<T>. A `fetch_*` inside a match/if arm is not the rhs's.
        top_level = not re.match(r"(?:match|if|while|for|unsafe|loop)\b", rhs)
        if resolved and top_level and re.search(r"fetch_(all|many)", rhs):
            resolved = f"Vec<{resolved}>"
        elif resolved and top_level and "fetch_optional" in rhs:
            resolved = f"Option<{resolved}>"
        if resolved:
            rust[name] = resolved
        if resolved and (bound := pg_resolved(resolved)):
            pg[name] = bound
        elif inferred != "unknown":
            pg[name] = inferred
    # `let Json(x) = match payload { Ok(v) => v, … }` — the extractor
    # destructure gives x the payload's unwrapped type.
    for m in re.finditer(
        r"let\s+(?:Json|Form|Path|Query|State|Extension)\((\w+)\)\s*=\s*(?:match\s+)?([A-Za-z_]\w*)",
        body,
    ):
        name, source = m.groups()
        if source_ty := rust.get(source):
            rust[name] = strip_wrappers(source_ty)
            if resolved := pg_resolved(rust[name]):
                pg[name] = resolved
    # `if let Some(x) = scrutinee {` — x binds the scrutinee's payload for
    # the enclosing block (same shape as a match `Some` arm).
    for m in re.finditer(
        r"if\s+let\s+Some\((\w+)\)\s*=\s*([^;{]+?)\s*\{", body
    ):
        name, scrutinee = m.group(1), m.group(2).strip()
        if scrut_ty := _rust_expr(scrutinee, rust) or rust.get(scrutinee):
            rust.setdefault(name, strip_wrappers(scrut_ty))
            if resolved := pg_resolved(rust[name]):
                pg.setdefault(name, resolved)
    # `let Some(x) = y else …` / `let Ok(x) = y else …` unwraps one Option/
    # Result level — y's recorded type is already the inner type when it came
    # from a query turbofish, and Option<T>/Result<T> binds as T either way.
    for m in re.finditer(r"let\s+(?:Some|Ok)\((\w+)\)\s*=\s*([^;]+?)\s+else", body):
        # `if let`/`while let` end in `{…} else` too; their "source" would
        # swallow the whole then-block. The `if let` scan above owns those.
        if body[: m.start()].rstrip().endswith(("if", "while")):
            continue
        name, source = m.groups()
        source = source.strip()
        source_ty = (
            rust.get(source)
            or _rust_expr(source, rust)
            or _rust_of(source, rust, pg)
        )
        if source_ty:
            # Shadowing semantics: the let rebinds, so overwrite any earlier
            # `name` type (`Path(event_id): Path<String>` → `let Ok(event_id)`
            # rebinds it as Uuid).
            inner = _option_inner(strip_wrappers(source_ty))
            rust[name] = inner or strip_wrappers(source_ty)
            if resolved := pg_resolved(rust[name]):
                pg[name] = resolved
            else:
                pg.pop(name, None)
    # `match expr { Ok(Some(x)) => …` / `Some(x) =>` / `Ok(x) =>` arm
    # bindings — x's type is the scrutinee's payload.
    arms = []
    for m in re.finditer(
        r"(?:Ok\(Some\((\w+)\)\)|Some\((\w+)\)|Ok\((\w+)\))\s*=>", body
    ):
        arms.append((m.start(), m.group(1) or m.group(2) or m.group(3)))
    matches = [
        (m.start(), m.group(1)) for m in re.finditer(r"match\s+([^\{]+?)\s*\{", body)
    ]
    for arm_pos, name in arms:
        scrutinee = next((s for pos, s in reversed(matches) if pos < arm_pos), None)
        if scrutinee and (scrut_ty := _rust_expr(scrutinee, rust)):
            rust.setdefault(name, strip_wrappers(scrut_ty))
            if resolved := pg_resolved(rust[name]):
                pg.setdefault(name, resolved)
    # `let (a, b) = expr` — tuple destructures: a tuple variable, a
    # `match` on a tuple/Option scrutinee, or a `query_as::<_, (A, B)>` row.
    for m in re.finditer(r"let\s*\(([^)]*)\)\s*=\s*([^;]+?)\s*;", body):
        names = [n.strip() for n in m.group(1).split(",")]
        rhs = m.group(2).strip()
        source_ty = _rust_expr(rhs, rust) or _rust_of(rhs, rust, pg)
        if source_ty and not re.match(
            r"(?:match|if|while|for|unsafe|loop)\b", rhs
        ) and re.search(r"fetch_(all|many)", rhs):
            source_ty = f"Vec<{source_ty}>"
        if not source_ty:
            continue
        elem = _element_rust(source_ty) or strip_wrappers(source_ty)
        if elem.startswith("(") and elem.endswith(")"):
            for name, part in zip(names, _top_level_commas(elem[1:-1])):
                if name and name != "_":
                    rust[name] = part.strip()
                    if resolved := pg_resolved(part):
                        pg[name] = resolved
        elif names:
            # `let (a, b) = match option { Some(x) => (x, true), … }` — only
            # the first bound name can be tied back to the scrutinee's type.
            rust.setdefault(names[0], elem)
            if resolved := pg_resolved(elem):
                pg.setdefault(names[0], resolved)
    # `let Struct { field, other: renamed } = value` — field types off the
    # value's declared struct type.
    for m in re.finditer(
        r"let\s+\w+\s*\{([^}]*)\}\s*=\s*([A-Za-z_]\w*)", body
    ):
        fields_list, source = m.groups()
        source_ty = rust.get(source)
        if not source_ty:
            continue
        for item in fields_list.split(","):
            item = item.strip()
            if not item or item == "..":
                continue
            field, _, name = item.partition(":")
            field = field.strip()
            name = (name.strip() or field)
            if field_ty := _field_type(source_ty, field):
                rust[name] = field_ty
                if resolved := pg_resolved(field_ty):
                    pg[name] = resolved
    # `v.push(T {…})` — reveals the element type of an otherwise
    # unresolvable `let mut v = Vec::…` so `for x in &v` still types.
    for m in re.finditer(r"(\w+)\.push\(\s*(\w+)\s*\{", body):
        vec_name, item = m.groups()
        if strip_wrappers(rust.get(vec_name, "")) in ("", "Vec", "Vec<_>"):
            rust[vec_name] = f"Vec<{item}>"
    for m in re.finditer(
        r"for\s+(?:mut\s+)?(\w+)\s+in\s+(?:&\s*(?:mut\s+)?)?([A-Za-z_][\w\.]*(?:\.\w+\(\))*)", body
    ):
        name, iterable = m.group(1), m.group(2)
        if element := _for_element(iterable, rust):
            rust[name] = element
            if resolved := pg_resolved(element):
                pg[name] = resolved
    for m in re.finditer(
        r"for\s*\(([^)]*)\)\s*in\s+(?:&\s*(?:mut\s+)?)?([A-Za-z_][\w\.]*(?:\.\w+\(\))*)", body
    ):
        names, iterable = m.groups()
        source_ty = _for_element(iterable, rust)
        if source_ty:
            stripped = strip_wrappers(source_ty)
            if stripped.startswith("(") and stripped.endswith(")"):
                parts = _top_level_commas(stripped[1:-1])
                for name, part in zip(
                    [n.strip() for n in names[0].split(",")], parts
                ):
                    if name and name != "_":
                        rust[name] = part.strip()
                        if resolved := pg_resolved(part):
                            pg[name] = resolved
    return pg, rust


def _member_rust(argument: str, declared_rust: dict[str, str]) -> str | None:
    m = re.fullmatch(r"(self|\w+)((?:\.\w+)+)", argument, re.S)
    if not m:
        return None
    rust = declared_rust.get(m.group(1))
    for segment in m.group(2)[1:].split("."):
        if rust is None:
            return None
        rust = _field_type(rust, segment)
    return rust


_EXPR_TYPE_HINTS = (
    (r"(?:time::)?OffsetDateTime::now_utc\(\)", "OffsetDateTime"),
    (r"(?:std::time::)?SystemTime::now\(\)", "OffsetDateTime"),
    (r"(?:time::)?Date::(?:today|from_calendar_date)\b", "Date"),
    (r"Uuid::(?:now_v7|new_v4)\(\)", "Uuid"),
    (r"String::(?:new|from)\b", "String"),
    (r"format!\(", "String"),
    (r"serde_json::to_string\b", "String"),
    (r"serde_json::to_value\b|json!\(", "Value"),
)


def _rust_of(rhs: str, declared_rust: dict[str, str], declared_pg: dict[str, str]) -> str | None:
    """Best-effort Rust type for an unannotated `let` RHS — turbofish on
    `query_scalar::<_, T>` / `query_as::<_, Row>`, member and method chains,
    construction expressions and arithmetic on a typed base."""
    rhs = rhs.strip()
    if re.match(r"(?:match|if|while|for|unsafe|loop)\b", rhs):
        # Control-flow RHS: the generic turbofish/member searches below can
        # hit a query *inside* an arm — never let them. The only sound
        # inference is a scrutinee projection (`Some(v) => v`).
        scrutinee = None
        if rhs.startswith("match ") and (brace := rhs.find("{")) > 0:
            scrutinee = rhs[6:brace].strip()
        scrutinee_ty = scrutinee and _rust_expr(scrutinee, declared_rust)
        projections = re.findall(
            r"(?:Some|Ok)\(\s*(\w+)\s*\)\s*=>\s*(\w+)\s*[,}]", rhs
        )
        if scrutinee_ty and any(bound == var for var, bound in projections):
            stripped = strip_wrappers(scrutinee_ty)
            if stripped != scrutinee_ty:
                return stripped
        return None
    if m := re.search(r"::<\s*_\s*,\s*(.+?)\s*>", rhs):
        return m.group(1)
    for pattern, rust_ty in _EXPR_TYPE_HINTS:
        if re.match(pattern, rhs):
            return rust_ty
    if m := re.fullmatch(r"(self|\w+)((?:\.\w+)*)\.(\w+)\(.*\)", rhs, re.S):
        base, segments, method = m.groups()
        base_ty = declared_rust.get(base)
        if base_ty is not None:
            for segment in segments[1:].split(".") if segments else ():
                base_ty = _field_type(base_ty, segment)
                if base_ty is None:
                    break
            if base_ty is not None:
                ret = workspace_index()["methods"].get(
                    (strip_wrappers(base_ty).split("::")[-1], method)
                )
                if ret:
                    return ret
    if m := re.match(r"(\w+)\(", rhs):
        # `f(…)` only when the call is the whole rhs — `f(…).g(…)` is a
        # chain, not a bare `f` return.
        open_at = rhs.index("(")
        if _balanced(rhs, open_at) == len(rhs) - 1:
            if ret := workspace_index()["fn_returns"].get(m.group(1)):
                return ret
    if member := _member_rust(rhs, declared_rust):
        return member
    # `base + Duration`, `base - other`, `base * n`: the result keeps the
    # typed base's type (OffsetDateTime + Duration, i64 * 2, …).
    if m := re.match(r"([A-Za-z_]\w*)\s*[-+*/]", rhs):
        base = m.group(1)
        if base in declared_rust:
            return declared_rust[base]
    return None


def _alternatives(rhs: str) -> list[str] | None:
    """The candidate value expressions of an `if cond { A } else { B }` or
    `match { pat => A, … }` RHS — used when every arm agrees on a type."""
    if re.match(r"if\b", rhs):
        arms = re.findall(r"\{\s*([^{};]+?)\s*\}", rhs)
        return arms if len(arms) >= 2 else None
    if re.match(r"match\b", rhs):
        arms = re.findall(r"=>\s*([^,}\n]+)", rhs)
        return arms or None
    return None


def _infer_alternatives(
    rhs: str,
    constants: dict[str, str],
    declared: dict[str, str],
    declared_rust: dict[str, str],
) -> str | None:
    arms = _alternatives(rhs)
    if not arms:
        return None
    found = []
    for arm in arms:
        arm = arm.strip()
        if re.search(r"\b(?:return|break|continue|unreachable!|panic!)\b|\?", arm):
            continue  # Diverging arm — it does not contribute a value.
        found.append(infer(arm, constants, declared, declared_rust))
    known = {pg for pg in found if pg != "unknown"}
    return next(iter(known)) if found and len(known) == 1 and "unknown" not in found else None


def infer(
    argument: str,
    constants: dict[str, str],
    declared: dict[str, str],
    declared_rust: dict[str, str] | None = None,
) -> str:
    declared_rust = declared_rust or {}
    argument = argument.strip().lstrip("&")
    if argument.endswith((".into_uuid()", ".as_uuid()")) or argument in (
        "Uuid::now_v7()",
        "Uuid::new_v4()",
    ):
        return "uuid"
    for rust, pg in (
        ("i64", "bigint"),
        ("i32", "integer"),
        ("i16", "smallint"),
        ("u32", "integer"),
    ):
        if re.search(rf"^{rust}::(?:from|try_from)\(|\bas {rust}$", argument):
            return pg
    if re.search(r"^f64::from\(|\bas f64$", argument):
        return "double precision"
    if re.fullmatch(r'"(?:[^"\\]|\\.)*"', argument) or re.search(
        r"\.(?:as_str|to_string|trim|join)\(\)?$|\.join\([^)]*\)$", argument
    ):
        return "text"
    if argument.startswith(("json!", "serde_json::json!", "serde_json::to_value(")):
        return "jsonb"
    if argument.startswith(("format!", "serde_json::to_string")):
        return "text"
    if m := re.search(r"\.collect::<\s*(Vec<[^>]+>|\[\w+\])\s*>", argument):
        if pg := pg_type(m.group(1)):
            return pg
    if argument in ("true", "false"):
        return "boolean"
    if argument.startswith("matches!") and _balanced(
        argument, argument.index("(")
    ) == len(argument) - 1:
        return "boolean"
    if re.fullmatch(r"\d+", argument):
        return "integer"
    if m := re.fullmatch(r"\[(.*)\]", argument, re.S):
        elements = [
            infer(part, constants, declared, declared_rust)
            for part in _top_level_commas(m.group(1))
            if part.strip()
        ]
        if elements and len(set(elements)) == 1 and elements[0] != "unknown":
            return f"{elements[0]}[]"
    # Predicates: a simple operand, an operator, a simple operand — nothing
    # that would match a query/method chain containing `==` mid-expression.
    simple = r"[\w\.\(\)\[\]'\*&]+"
    if not re.match(r"(?:if|match|while|for|unsafe|loop)\b", argument) and (
        re.fullmatch(
            rf"{simple}\s*(==|!=|>=|<=)\s*{simple}|{simple}\s[<>]\s{simple}",
            argument,
        )
        or re.fullmatch(
            r"[A-Za-z_][\w\.]*\.(is_some|is_none|is_empty|contains|starts_with|ends_with|exists)\(.*\)",
            argument,
            re.S,
        )
        or re.fullmatch(r"!\s*[A-Za-z_][\w\.]*", argument)
        or re.fullmatch(rf"{simple}\s*(&&|\|\|)\s*{simple}", argument)
    ):
        return "boolean"
    if argument in constants:
        return constants[argument]
    if workspace_const := workspace_index()["consts"].get(argument):
        return pg_resolved(workspace_const) or "unknown"
    if argument in declared:
        return declared[argument]
    if m := re.fullmatch(r"Some\((.*)\)", argument, re.S):
        return infer(m.group(1), constants, declared, declared_rust)
    if m := re.search(r"Into::<\s*([\w:]+)\s*>::into\(", argument):
        if pg := pg_resolved(m.group(1)):
            return pg
    if m := re.search(r"\.collect::<\s*(Vec<[^>]+>|\[\w+\])\s*>", argument):
        if pg := pg_type(m.group(1)):
            return pg
    if m := re.fullmatch(r"(\w+)\.unwrap_or(?:_default|_else)?\(.*\)", argument, re.S):
        base = m.group(1)
        if base in declared_rust:
            if inner := re.fullmatch(r"Option<(.*)>", strip_wrappers(declared_rust[base])):
                return pg_resolved(inner.group(1)) or "unknown"
        return declared.get(base, "unknown")
    # `x.as_deref()` / `x.get(…)` / `x.map(T)` / `x.and_then(…)` — Option<T>
    # combinators; the bound value is T.
    if m := re.fullmatch(r"(self|\w+)((?:\.\w+)*)\.(as_deref|get|map|and_then|or)\((.*)\)", argument, re.S):
        base, segments, _, inner_args = m.groups()
        rust = declared_rust.get(base)
        for segment in segments[1:].split(".") if segments else ():
            if rust is None:
                break
            rust = _field_type(rust, segment)
        if rust is not None:
            stripped = strip_wrappers(rust)
            if inner := re.fullmatch(r"Option<(.*)>", stripped):
                return pg_resolved(inner.group(1)) or "unknown"
            if map_ty := re.fullmatch(r"([A-Z]\w*)", inner_args.strip()):
                return pg_resolved(map_ty.group(1)) or "unknown"
    if argument.endswith(".clone()"):
        return infer(argument[: -len(".clone()")], constants, declared, declared_rust)
    # `base + rhs`, `base * n` — arithmetic keeps a typed base's type:
    # OffsetDateTime + Duration, Date + days, i64 + 1.
    if m := re.match(r"([A-Za-z_]\w*)\s*[-+*/]", argument):
        base = m.group(1)
        if base in declared:
            return declared[base]
        if base in constants:
            return constants[base]
        if ws_const := workspace_index()["consts"].get(base):
            return pg_resolved(ws_const) or "unknown"
        if base in declared_rust:
            return pg_resolved(declared_rust[base]) or "unknown"
    if pg := _infer_alternatives(argument, constants, declared, declared_rust):
        return pg
    if rust := _rust_expr(argument, declared_rust):
        return pg_resolved(rust) or "unknown"
    return "unknown"


def _split_access(expr: str) -> tuple[str, list[tuple[str, str]]] | None:
    """`base.a.b(args)` → (`base`, [('field','a'), ('call','b','args')]).
    Returns None when the expression is not a plain access chain."""
    depth, i, base_end = 0, 0, len(expr)
    while i < len(expr):
        ch = expr[i]
        if ch in "(<[{":
            depth += 1
        elif ch in ")>]}":
            depth -= 1
        elif ch == "." and depth == 0:
            base_end = i
            break
        i += 1
    if i >= len(expr):
        return None
    base, tokens = expr[:base_end], []
    while i < len(expr):
        if expr[i] != ".":
            return None
        j = i + 1
        while j < len(expr) and (expr[j].isalnum() or expr[j] == "_"):
            j += 1
        name = expr[i + 1 : j]
        if not name:
            return None
        if j < len(expr) and expr[j] == "(":
            close = _balanced(expr, j)
            tokens.append(("call", name, expr[j + 1 : close - 1]))
            i = close
        else:
            tokens.append(("field", name))
            i = j
    return base, tokens


def _base_type(expr: str, declared_rust: dict[str, str]) -> str | None:
    expr = expr.strip().lstrip("&")
    if expr in declared_rust:
        return declared_rust[expr]
    if m := re.fullmatch(r"(\w+)\((.*)\)", expr, re.S):
        stored = declared_rust.get(m.group(1))
        if stored and stored.startswith("fn:"):
            return stored[4:-1]
    # `module::CONST` / `crate::CONST` — the workspace index records the bare
    # name; the last path segment is the lookup.
    if m := re.fullmatch(r"(?:\w+::)*([A-Z][A-Z0-9_]*)", expr):
        return workspace_index()["consts"].get(m.group(1))
    if re.fullmatch(r'"(?:[^"\\]|\\.)*"', expr) or expr.startswith(("format!", "String::")):
        return "String"
    if re.fullmatch(r"\d+", expr):
        return "i32"
    if expr in ("true", "false"):
        return "bool"
    if expr.startswith(("json!", "serde_json::json!")):
        return "Value"
    if m := re.fullmatch(r"(\w+)\((.*)\)", expr, re.S):
        return workspace_index()["fn_returns"].get(m.group(1))
    if expr.startswith(("matches!", "assert!", "assert_eq!")):
        return "bool"
    if re.fullmatch(r"(?:uuid::)?Uuid::(parse_str|from_str)\(.*\)", expr, re.S) or re.fullmatch(
        r"(\w+)::parse\(\)", expr
    ):
        return "Result<Uuid, UuidError>"
    if expr.startswith("env!"):
        return "String"
    if expr.startswith(("serde_json::Value::", "Value::")):
        return "Value"
    if expr.startswith(("Vec::new", "Vec::with_capacity", "Vec::default")):
        return "Vec"
    if m := re.fullmatch(r"vec!\[(.+?)(?:;\s*.+)?\]", expr, re.S):
        # `vec![x; n]` / `vec![x, y]` — element type decides the Vec<T>.
        element = m.group(1).split(",")[0].strip()
        inner_ty = _rust_expr(element, declared_rust)
        return f"Vec<{inner_ty or '_'}>"
    if m := re.fullmatch(r"(\w+)\[.*\]", expr):
        if rust := declared_rust.get(m.group(1)):
            return _element_rust(rust)
    if expr.startswith("(") and expr.endswith(")"):
        return _base_type(expr[1:-1], declared_rust)
    for pattern, rust_ty in _EXPR_TYPE_HINTS:
        if re.match(pattern, expr):
            return rust_ty
    return None


def _element_rust(rust: str) -> str | None:
    """Element type of a `Vec<T>` / `&[T]` / `Option<T>` receiver; the value
    type of a `HashMap<K, V>` / `BTreeMap<K, V>`; `Value` for a JSON `get`."""
    rust = strip_wrappers(rust)
    for wrapper in ("Vec", "HashSet", "BTreeSet", "VecDeque", "Option"):
        if m := re.fullmatch(rf"{wrapper}<(.*)>", rust):
            return m.group(1)
    for mapping in ("HashMap", "BTreeMap", "HashMap<String"):
        if m := re.fullmatch(rf"{mapping}<(.*)>", rust):
            args = _top_level_commas(m.group(1))
            return args[-1].strip() if args else None
    if m := re.fullmatch(r"\[(\w+)(?:;\s*\d+)?\]", rust):
        return m.group(1)
    if rust in ("String", "str"):
        return "str"
    if rust in ("Value", "serde_json::Value"):
        return "Value"
    return None


def _option_inner(rust: str) -> str | None:
    if m := re.fullmatch(r"Option<(.*)>", strip_wrappers(rust)):
        return m.group(1)
    return None


def _apply_access(
    rust: str, token: tuple[str, str], declared_rust: dict[str, str] | None = None
) -> str | None:
    declared_rust = declared_rust or {}
    kind, name = token[0], token[1]
    if kind == "field":
        if name == "await":
            return rust
        return _field_type(rust, name)
    args = token[2] if len(token) > 2 else ""
    inner = _option_inner(rust)
    # Option<T> and collection combinators whose result type is mechanical.
    if name in ("unwrap", "unwrap_or", "unwrap_or_default", "unwrap_or_else", "expect"):
        return inner or rust
    if name in ("ok_or", "ok_or_else"):
        return f"Result<{inner or strip_wrappers(rust)}>"
    if name == "transpose":
        return rust
    if name in ("ok", "err"):
        # `Result::ok()/err()` → Option<the corresponding half>.
        return f"Option<{inner or strip_wrappers(rust)}>"
    if name in ("cloned", "copied", "get"):
        element = _element_rust(strip_wrappers(rust))
        if element:
            return f"Option<{element}>"
        return f"Option<{strip_wrappers(rust)}>" if inner else None
    if name == "as_deref":
        # `Option<Vec<T>>::as_deref()` binds `&[T]` (an array), not `T` —
        # while `Option<String>::as_deref()` binds `&str`.
        stripped = strip_wrappers(rust)
        if m := re.fullmatch(r"(?:Vec|HashSet|BTreeSet|VecDeque)<(.*)>", stripped, re.S):
            return f"Option<[{m.group(1).strip()}]>"
        if stripped in ("String", "str"):
            return "Option<str>"
        if element := _element_rust(stripped):
            return f"Option<{element}>"
        return f"Option<{stripped}>" if inner else None
    if name in ("as_ref", "filter", "iter", "iter_mut", "into_iter"):
        return rust
    if name in ("then", "then_some"):
        # `bool.then(|| v)` / `then_some(v)` → Option<T>.
        body = args.strip()
        if name == "then" and (m := re.match(r"^\|[^|]*\|\s*(.+)$", body, re.S)):
            inner_ty = _rust_expr(m.group(1).strip(), declared_rust)
            if inner_ty:
                return f"Option<{inner_ty}>"
        if name == "then_some" and (inner_ty := _rust_expr(body, declared_rust)):
            return f"Option<{inner_ty}>"
        return None
    if name in ("map", "and_then", "map_or"):
        arg = args.strip()
        if m := re.fullmatch(r"([A-Z]\w*)::(\w+)", arg):
            if ret := workspace_index()["methods"].get((m.group(1), m.group(2))):
                return f"Option<{ret}>" if inner else ret
        if m := re.fullmatch(r"([A-Z]\w*)", arg):
            return f"Option<{m.group(1)}>" if inner else m.group(1)
        if "into_uuid()" in arg or "as_uuid()" in arg:
            return "Option<Uuid>" if inner else "Uuid"
        if closure := re.fullmatch(r"\|(\w+)\|\s*(.+)", arg, re.S):
            # `|x| x.field.as_deref()` / `|x| f(x)` — x binds the receiver's
            # element type, then the body evaluates as an expression under it.
            param, body = closure.group(1), closure.group(2).strip()
            element = _element_rust(strip_wrappers(rust))
            if element:
                resolved = _rust_expr(
                    body, {**declared_rust, param: element}
                )
                if resolved:
                    # `map` wraps; `and_then` adopts the body's Option.
                    if resolved.startswith("Option<"):
                        return resolved
                    return f"Option<{resolved}>"
        if closure := re.fullmatch(r"\|\((\w+),\s*(\w+)\)\|\s*\*?(\w+)", arg):  # noqa: E501
            # `|(at, _)| *at` on an Option<(A, B)> — the picked slot's type.
            stripped = strip_wrappers(rust)
            elem = _option_inner(stripped) or stripped
            if elem.startswith("(") and elem.endswith(")"):
                parts = _top_level_commas(elem[1:-1])
                picked = 0 if closure.group(3) == closure.group(1) else 1
                if picked < len(parts):
                    return f"Option<{parts[picked].strip()}>"
        return None
    if name == "into_uuid" or name == "as_uuid":
        return "Uuid"
    if name in ("to_string", "to_string_lossy", "as_str", "trim", "to_owned", "join",
                "to_lowercase", "to_uppercase", "trim_end_matches", "trim_start_matches",
                "trim_matches", "replace", "replacen", "to_ascii_uppercase",
                "to_ascii_lowercase"):
        return "String"
    if name in ("strip_prefix", "strip_suffix"):
        return "String"
    if name == "clone":
        return rust
    if name in ("to_vec", "as_slice"):
        element = _element_rust(strip_wrappers(rust))
        return f"Vec<{element}>" if element else None
    if name in ("clamp", "min", "max", "abs", "saturating_add", "saturating_sub",
                "saturating_mul", "checked_add", "checked_sub"):
        return rust
    if name == "to_vec" or name == "collect":
        element = _element_rust(strip_wrappers(rust))
        return f"Vec<{element}>" if element else None
    if name == "date":
        return "Date"
    if name == "time":
        return "time::Time"
    if name in ("whole_seconds", "whole_milliseconds", "whole_days", "unix_timestamp"):
        return "i64"
    if name in ("is_some", "is_none", "is_empty", "exists", "contains", "starts_with", "ends_with"):
        return "bool"
    if name == "await":
        return rust
    # Anything else: the impl's declared return type.
    ret = workspace_index()["methods"].get((strip_wrappers(rust).split("::")[-1], name))
    if ret:
        return ret
    # An Option<T>.method() that the impl index does not know still unwraps
    # through `_option_inner` on the next segment only when it is one of the
    # mechanical combinators above; otherwise the chain dies here.
    return None


def _rust_expr(expr: str, declared_rust: dict[str, str]) -> str | None:
    """The Rust type of a bind expression — the declared variable, member and
    method chains, Option combinators, `Some(…)`, turbofish and `?`."""
    expr = expr.strip().lstrip("&")
    if re.match(r"(?:if|match|while|for|unsafe|loop)\b", expr):
        # Control-flow blocks can hold queries and typed calls whose
        # turbofish/method return types are not the block's value.
        return None
    if expr.endswith("?"):
        inner = _rust_expr(expr[:-1], declared_rust)
        if not inner:
            return None
        core = inner.strip()
        # `?` peels exactly one wrapper: `Result<Option<T>, E>?` is
        # `Option<T>`, not `T` — stripping all wrappers would over-unwrap.
        if re.fullmatch(r"(?:Result|Option)<.*>", core, re.S):
            args = _generic_args(core)
            return args[0].strip() if args else None
        return core
    if m := re.fullmatch(r"Some\((.*)\)", expr, re.S):
        inner = _rust_expr(m.group(1), declared_rust)
        return f"Option<{inner}>" if inner else None
    if m := re.search(r"Into::<\s*([\w:]+)\s*>::into\(", expr):
        return m.group(1)
    if m := re.search(r"::<\s*_\s*,\s*(.+?)\s*>", expr):
        return m.group(1)
    if m := re.search(r"::<\s*([A-Z][\w:]*)\s*>", expr):
        return m.group(1)
    split = _split_access(expr)
    if split is None:
        return _base_type(expr, declared_rust)
    base, tokens = split
    rust = _base_type(base, declared_rust)
    for token in tokens:
        if rust is None:
            return None
        rust = _apply_access(rust, token, declared_rust)
    return rust


def statements() -> list[tuple[str, int, str, list[str]]]:
    global _CUR_FIELDS
    found = []
    by_file = workspace_index()["fields_by_file"]
    for path in sorted((ROOT / "crates").rglob("*.rs")):
        relative = path.relative_to(ROOT).as_posix()
        if _types.is_test_source(relative):
            continue
        _CUR_FIELDS = by_file.get(relative, {})
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
            declared, declared_rust = declared_types(text, literal.start())
            types = [
                infer(argument, constants, declared, declared_rust)
                for argument in arguments[:parameters]
            ]
            if all(pg == "unknown" for pg in types):
                continue
            line = text[: literal.start()].count("\n") + 1
            found.append((relative, line, sql, types))
    _CUR_FIELDS = {}
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
