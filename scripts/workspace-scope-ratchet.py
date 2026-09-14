#!/usr/bin/env python3
"""No new query may read a workspace-scoped table without naming the workspace.

232 tables carry a `workspace_id`. That column is the whole of CrowdRelay's
tenant isolation: a process serves exactly one workspace (`CROWDRELAY_WORKSPACE_SLUG`
is a single process-level variable), so a query that forgets the predicate reads
whatever else is in the database.

Today that cannot leak, because one deployment has one workspace and its own
Postgres. That is a property of the current deployment, not of the code, and it
is the property a second organization removes. The gap is already real in one
place: `seconds_since_last_observation` aggregated `community_observations` with
no predicate, so a second workspace's observations would have satisfied this
one's freshness check and postponed its discovery sweep by a full interval, with
nothing in the log to say why.

A ratchet rather than a hard rule, following `source-size-ratchet.py` and
`api-sql-ratchet.py`. Most of the recorded statements are benign -- a write keyed
by a primary key the caller just read from a scoped query is not a leak, and
rewriting dozens of those tonight would be churn with no behaviour change. What
matters is that the number cannot grow: a new unscoped statement fails, and the
baseline may shrink freely.

Not a substitute for reading the query. A statement can name `workspace_id` and
still be wrong -- binding the wrong one, or scoping an outer query while an inner
subquery roams. This catches the version that is mechanically detectable, which
is the one that gets written by accident.

**Both string forms, since 2026-09-14.** This used to inspect only `r#"..."#`
literals, on a stated assumption -- "which is how every query in this workspace is
written" -- that was measurably false: 83 candidate statements live in plain
`"..."` literals against 44 in raw ones, and 15 of those were in production files
the ratchet could not see at all. An assumption in a gate's own docstring is worth
checking, because the gate is exactly as good as it.

Widening it admitted those 15 into the baseline. Each was read first: twelve in
`growth_readiness.rs` are process-level "has this component done anything at all"
probes, two in `audience/query_support.rs` carry their workspace predicate inside
an interpolated `{}` fragment this scan cannot see, and one in `reminders.rs` is
keyed by a primary key already read from a scoped query. None is a leak today, and
none may now grow.

`src/**/tests.rs` is excluded too. The old exclusion tested for `/tests/` in the
path, which catches an integration target under `crates/*/tests/` and misses the
unit-test modules this workspace keeps beside their source -- so widening to plain
strings would otherwise have counted 19 statements in `retention/tests.rs` and
`community_executor/tests.rs`, where a fixture reaching across workspaces is doing
its job.
"""
from __future__ import annotations

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BASELINE = Path(__file__).with_suffix(".json")
MIGRATIONS = ROOT / "migrations"
CRATES = ROOT / "crates"


def scoped_tables() -> set[str]:
    """Tables whose CREATE TABLE declares a workspace_id column."""
    sql = "".join(
        path.read_text(encoding="utf-8", errors="replace")
        for path in sorted(MIGRATIONS.glob("*.sql"))
    )
    tables = set()
    for match in re.finditer(
        r"CREATE TABLE (?:IF NOT EXISTS )?([a-z0-9_]+)\s*\((.*?)\n\);", sql, re.S
    ):
        if re.search(r"\bworkspace_id\b", match.group(2)):
            tables.add(match.group(1))
    return tables


#: A raw literal, `r#"..."#`. Most queries here are written this way.
RAW_LITERAL = re.compile(r'r#"(.*?)"#', re.S)

#: A plain literal, `"..."`, honouring escapes. Also carries queries, and used to
#: be invisible to this gate.
PLAIN_LITERAL = re.compile(r'"((?:[^"\\]|\\.)*)"', re.S)


def statements(text: str) -> list[str]:
    """Every SQL-ish string literal in a source file, each counted once.

    The two patterns overlap and that matters: `PLAIN_LITERAL` also matches the
    body of a raw literal, because `r#"SELECT ..."#` contains a `"`, the query,
    and another `"`. Scanning both over the same text double-counted every raw
    query -- `community_executor.rs` read 15 where it has 7 -- which would have
    demanded a doubled baseline and hidden any real growth underneath the noise.
    So raw literals are extracted first and blanked out before the plain scan.
    """
    raw = RAW_LITERAL.findall(text)
    remainder = RAW_LITERAL.sub("", text)
    return raw + PLAIN_LITERAL.findall(remainder)


def is_test_source(relative: str) -> bool:
    """Test code, in either of the two places this workspace keeps it.

    `crates/*/tests/` holds integration targets. Unit tests live beside their
    source as `src/**/tests.rs` and are attached with `include!`, so a path test
    for `/tests/` alone misses them. A fixture reading across workspaces is doing
    its job in both.
    """
    return "/tests/" in relative or relative.endswith("/tests.rs")


def unscoped_statements(tables: set[str]) -> dict[str, int]:
    """Per-file count of statements naming a scoped table with no workspace_id."""
    counts: dict[str, int] = {}
    for path in sorted(CRATES.rglob("*.rs")):
        relative = path.relative_to(ROOT).as_posix()
        if is_test_source(relative):
            continue
        text = path.read_text(encoding="utf-8", errors="replace")
        hits = 0
        for statement in statements(text):
            lowered = statement.lower()
            if not re.search(r"\b(select|update|delete)\b", lowered):
                continue
            if "workspace_id" in lowered:
                continue
            if any(re.search(rf"\b{table}\b", lowered) for table in tables):
                hits += 1
        if hits:
            counts[relative] = hits
    return counts


def main() -> int:
    # `--write-baseline` re-records the current counts. Legitimate after scoping
    # a query (the number shrinks) and a decision that needs saying out loud
    # after adding one, which is why it is a flag and not what a bare run does.
    write_baseline = "--write-baseline" in sys.argv
    tables = scoped_tables()
    if len(tables) < 100:
        print(
            f"WORKSPACE_SCOPE_RATCHET=FAIL only {len(tables)} scoped tables found; "
            "the migration parser rotted",
            file=sys.stderr,
        )
        return 1

    current = unscoped_statements(tables)
    if write_baseline:
        BASELINE.write_text(json.dumps(dict(sorted(current.items())), indent=2) + "\n")
        print(
            f"WORKSPACE_SCOPE_RATCHET=BASELINE_WRITTEN "
            f"unscoped_statements={sum(current.values())} files={len(current)}"
        )
        return 0
    baseline: dict[str, int] = json.loads(BASELINE.read_text()) if BASELINE.is_file() else {}

    failures = []
    for path, count in sorted(current.items()):
        allowed = baseline.get(path, 0)
        if count > allowed:
            failures.append(
                f"  {path}: {count} unscoped statements, baseline allows {allowed}"
            )

    if failures:
        print("WORKSPACE_SCOPE_RATCHET=FAIL", file=sys.stderr)
        print(
            "These files gained a query that reads a workspace-scoped table without "
            "naming the workspace:",
            file=sys.stderr,
        )
        for line in failures:
            print(line, file=sys.stderr)
        print(
            "Add `WHERE workspace_id = $n` and bind the workspace this process serves. "
            "Raise the baseline only if the statement genuinely cannot be scoped, and "
            "say why in review.",
            file=sys.stderr,
        )
        return 1

    total = sum(current.values())
    shrunk = sum(baseline.values()) - total
    print(
        f"WORKSPACE_SCOPE_RATCHET=PASS scoped_tables={len(tables)} "
        f"unscoped_statements={total} files={len(current)}"
        + (f" shrunk_by={shrunk}" if shrunk > 0 else "")
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
