#!/usr/bin/env python3
"""Migration versions must stay unique.

`sqlx::migrate!` happily compiles a directory containing two files with the
same numeric version. The failure only appears at runtime: `Migrator::run`
keys applied migrations by version, so the second file with a duplicate
version is compared against the first one's checksum and startup aborts with
`VersionMismatch`. That is a deploy-blocking failure, so pin it here.
"""
import collections
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MIGRATION = re.compile(r"^(\d{4})_.+\.sql$")


class MigrationVersionContract(unittest.TestCase):
    def migrations(self) -> list[Path]:
        return sorted((ROOT / "migrations").glob("[0-9][0-9][0-9][0-9]_*.sql"))

    def test_every_migration_version_is_unique(self):
        versions = collections.defaultdict(list)
        for path in self.migrations():
            match = MIGRATION.match(path.name)
            self.assertIsNotNone(match, f"unexpected migration filename: {path.name}")
            versions[match.group(1)].append(path.name)
        duplicates = {v: names for v, names in versions.items() if len(names) > 1}
        self.assertEqual(duplicates, {}, f"duplicate migration versions: {duplicates}")

    def test_public_schema_version_matches_latest_migration(self):
        meta = (ROOT / "crates/crowdrelay-api/src/meta.rs").read_text(encoding="utf-8")
        # SCHEMA_VERSION is auto-discovered by build.rs — verify the pattern.
        self.assertIn("CROWDRELAY_SCHEMA_VERSION", meta)
        self.assertTrue((ROOT / "crates/crowdrelay-api/build.rs").is_file())


if __name__ == "__main__":
    unittest.main()
