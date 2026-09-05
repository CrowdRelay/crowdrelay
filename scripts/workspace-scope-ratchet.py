#!/usr/bin/env python3
"""No new query may read a workspace-scoped table without naming the workspace.

227 tables carry a `workspace_id`. That column is the whole of CrowdRelay's
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


def unscoped_statements(tables: set[str]) -> dict[str, int]:
    """Per-file count of statements naming a scoped table with no workspace_id.

    Only raw-string SQL literals are considered, which is how every query in
    this workspace is written. Test sources are excluded: a fixture inserting
    or asserting across workspaces is doing its job.
    """
    counts: dict[str, int] = {}
    for path in sorted(CRATES.rglob("*.rs")):
        relative = path.relative_to(ROOT).as_posix()
        if "/tests/" in relative:
            continue
        text = path.read_text(encoding="utf-8", errors="replace")
        hits = 0
        for statement in re.findall(r'r#"(.*?)"#', text, re.S):
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
