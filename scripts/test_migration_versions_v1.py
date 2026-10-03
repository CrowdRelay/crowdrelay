#!/usr/bin/env python3
"""Every migration file carries a version no other file carries.

sqlx keys `_sqlx_migrations` on the numeric prefix. Two files that share one
fail at `setup` with `duplicate key value violates unique constraint
"_sqlx_migrations_pkey"` — on a fresh database and on a deploy alike. Two
sessions each taking "the next number" produced exactly that on 2026-10-03
(0423 twice); nothing in CI looked at the numbers until a database was
migrated. A new gap is a decision rather than an accident; the two
historical ones are listed.
"""
from __future__ import annotations

import re
import unittest
from collections import Counter
from pathlib import Path

MIGRATIONS = Path(__file__).resolve().parents[1] / "migrations"
# Versions that were never used (measured 2026-10-03: `ls migrations`).
KNOWN_GAPS = {105, 169}
NAME = re.compile(r"^(\d{4})_[a-z0-9_]+\.sql$")


class MigrationVersions(unittest.TestCase):
    def versions(self) -> list[int]:
        files = sorted(p.name for p in MIGRATIONS.glob("*.sql"))
        self.assertGreater(len(files), 100, "migration discovery collapsed")
        bad = [name for name in files if not NAME.match(name)]
        self.assertEqual(bad, [], f"migration names must be NNNN_snake_case.sql: {bad}")
        return [int(NAME.match(name).group(1)) for name in files]

    def test_no_two_files_share_a_version(self) -> None:
        duplicated = sorted(v for v, n in Counter(self.versions()).items() if n > 1)
        self.assertEqual(duplicated, [], f"versions used by more than one file: {duplicated}")

    def test_versions_are_consecutive(self) -> None:
        versions = sorted(set(self.versions()))
        present = set(versions)
        gaps = {v for v in range(versions[0], versions[-1] + 1) if v not in present}
        self.assertEqual(gaps - KNOWN_GAPS, set(), f"new missing versions: {sorted(gaps - KNOWN_GAPS)}")


if __name__ == "__main__":
    unittest.main()
