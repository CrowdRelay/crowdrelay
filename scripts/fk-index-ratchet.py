#!/usr/bin/env python3
"""Foreign keys whose referencing side has no index — a ratchet.

PostgreSQL indexes the referenced side of a foreign key (it must be unique)
and never the referencing side. Without an index there, every DELETE of a
parent row scans the whole child table once to check the RESTRICT or to run
the CASCADE, and every join from parent to child does the same. The cost is
invisible on a development database and grows with the child table.

The 2026-09-27 SQL audit found 157 such keys. Most parents are never deleted
(workspaces, fans, cities), so those keys cost nothing and are recorded in the
baseline rather than indexed for the sake of a number. Where code does delete
the parent — the outbox retention sweep above all, which deletes in batches
and whose own NOT EXISTS probes read the same columns — 0370 added the
index. This gate keeps the list from growing: a new foreign key without a
covering index fails, naming itself; a key that gains one must be removed
from the baseline in the same change (`--write-baseline`), so the ratchet
fails in both directions.

A covering index is one whose leading columns are exactly the key's columns,
in any order, and that is either total or partial only on `col IS NOT NULL`
for those columns — a foreign-key lookup never searches for NULL, so such a
partial index serves it.

Needs a migrated schema, like `sql-result-types.py`; skips without one.
"""
from __future__ import annotations

import importlib.util
import json
import re
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
BASELINE = HERE / "fk-index-ratchet.json"

_spec = importlib.util.spec_from_file_location("sql_result_types", HERE / "sql-result-types.py")
_srt = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_srt)

QUERY = r"""
SELECT c.conrelid::regclass::text,
       array_to_string(array(
           SELECT a.attname FROM unnest(c.conkey) WITH ORDINALITY k(n, o)
           JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.n
           ORDER BY o), ','),
       c.confrelid::regclass::text,
       COALESCE((
           SELECT string_agg(
               array_to_string(array(
                   SELECT a.attname
                   FROM unnest(i.indkey::int2[]) WITH ORDINALITY k(n, o)
                   JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.n
                   WHERE o <= array_length(c.conkey, 1)
                   ORDER BY o), ',')
               || '|' || COALESCE(pg_get_expr(i.indpred, i.indrelid), ''),
               ';')
           FROM pg_index i
           WHERE i.indrelid = c.conrelid
       ), '')
FROM pg_constraint c
JOIN pg_namespace n ON n.oid = c.connamespace
WHERE c.contype = 'f' AND n.nspname = 'public'
ORDER BY 1, 2
"""

NOT_NULL = re.compile(r"^\(?(\w+) IS NOT NULL\)?$")


def covered(key_columns: set[str], indexes: str) -> bool:
    for entry in filter(None, indexes.split(";")):
        leading, _, predicate = entry.partition("|")
        if set(filter(None, leading.split(","))) != key_columns:
            continue
        if not predicate:
            return True
        clauses = [part.strip() for part in re.split(r"\) AND \(", predicate.strip("()"))]
        if all(
            (m := NOT_NULL.match(clause.strip("()"))) and m.group(1) in key_columns
            for clause in clauses
        ):
            return True
    return False


def unindexed(container: str) -> list[str]:
    result = _srt.psql(QUERY, container)
    if result.returncode != 0:
        raise SystemExit(f"FK_INDEX_RATCHET=ERROR {result.stderr.strip()}")
    found = []
    for line in result.stdout.splitlines():
        child, columns, parent, indexes = line.split("|", 3)
        if not covered(set(columns.split(",")), indexes):
            found.append(f"{child}({columns}) -> {parent}")
    return sorted(found)


def main() -> int:
    container = _srt.find_container()
    if not container:
        print("FK_INDEX_RATCHET=SKIP reason=no-local-database")
        return 0
    current = unindexed(container)
    if "--write-baseline" in sys.argv:
        BASELINE.write_text(json.dumps({"unindexed": current}, indent=2) + "\n")
        print(f"FK_INDEX_RATCHET=BASELINE unindexed={len(current)}")
        return 0
    known = json.loads(BASELINE.read_text())["unindexed"]
    new = [key for key in current if key not in known]
    gone = [key for key in known if key not in current]
    if new:
        print("FK_INDEX_RATCHET=FAIL — foreign keys with no index on the referencing side:")
        for key in new:
            print(f"  {key}")
        print(
            "Index the child columns in the migration that adds the key. If the parent is "
            "never deleted and the key is never joined from the parent side, say so in "
            "review and re-record with --write-baseline."
        )
        return 1
    if gone:
        print("FK_INDEX_RATCHET=FAIL — these keys are indexed now; lower the baseline:")
        for key in gone:
            print(f"  {key}")
        print("python3 scripts/fk-index-ratchet.py --write-baseline")
        return 1
    print(f"FK_INDEX_RATCHET=PASS unindexed={len(current)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
