"""Contract test: platform vocabulary must be synchronized across all three sources.

This test prevents the recurring bug where a new platform is added to
SYNCED_PLATFORMS and one (but not both) of the database CHECK constraints.
This has happened three times: Facebook, SoundCloud, and almost TikTok.

Three platform vocabularies must stay aligned:

1. SYNCED_PLATFORMS — platforms the worker looks for in fanbase_connections
2. fanbase_connections_platform_check — platforms allowed in the DB
3. viryaos_growth_metric_series_platform_check — metric platforms allowed in the DB

Invariants enforced:

- Every SYNCED_PLATFORMS entry must be in fanbase_connections_platform_check
  (so connections can be found for sync).
- Every platform string passed to record_metric_point() must be in
  viryaos_growth_metric_series_platform_check (so metrics can be recorded).
  Note: this is NOT the same as SYNCED_PLATFORMS — e.g. Reddit syncs from
  connections with platform='reddit' but records metrics as platform='social'.

The test is **migration-aware**: it processes all migration files in order
and tracks DROP CONSTRAINT / ADD CONSTRAINT pairs to compute the *effective*
constraint, not just the first or last one found.
"""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MIGRATIONS = ROOT / "migrations"
SYNC_SOURCE = ROOT / "crates" / "crowdrelay-worker" / "src" / "growth_metric_sync.rs"


def parse_synced_platforms(source: str) -> set[str]:
    """Extract platform strings from the SYNCED_PLATFORMS const."""
    match = re.search(
        r"const\s+SYNCED_PLATFORMS\s*:\s*&\[&str\]\s*=\s*&\[(.*?)\];",
        source,
        re.DOTALL,
    )
    if not match:
        raise AssertionError("SYNCED_PLATFORMS not found in growth_metric_sync.rs")
    platforms = re.findall(r'"([^"]+)"', match.group(1))
    return set(platforms)


def parse_metric_platforms(source: str) -> set[str]:
    """Extract platform strings passed to record_metric_point() calls.

    record_metric_point is called with a string literal as the 4th argument
    (after pool, workspace_id, conn.id). We extract all such literals.
    """
    # Match: record_metric_point( ... "platform" ...
    # The platform is the 4th positional arg, always a string literal.
    platforms = set()
    for match in re.finditer(r"record_metric_point\s*\(", source):
        # Find the string literal that's the platform argument.
        # We look for the pattern: ,\s*"([^"]+)",\s*"
        # after the function call — the first quoted string after conn.id
        # is the platform, the second is the metric_key.
        rest = source[match.start():]
        # Find the first occurrence of a string between commas after the
        # pool/workspace/conn arguments. The platform arg is followed by
        # the metric_key arg, both string literals.
        inner = re.search(
            r'record_metric_point\s*\([^)]*?,\s*"([^"]+)",\s*"([^"]+)"',
            rest,
            re.DOTALL,
        )
        if inner:
            platforms.add(inner.group(1))
    return platforms


def parse_effective_trigger_body() -> str:
    """The body of the last notify_growth_metric_sync() definition.

    The function is redefined with CREATE OR REPLACE, so the last definition
    across migrations in order is the one the database ends up running.
    """
    body = ""
    for migration in sorted(MIGRATIONS.glob("*.sql")):
        text = migration.read_text()
        for match in re.finditer(
            r"CREATE\s+OR\s+REPLACE\s+FUNCTION\s+notify_growth_metric_sync\b(.*?)\$\$;",
            text,
            re.IGNORECASE | re.DOTALL,
        ):
            body = match.group(1)
    if not body:
        raise AssertionError(
            "notify_growth_metric_sync() not found in any migration"
        )
    return body


def parse_effective_constraint(constraint_name: str) -> set[str]:
    """Process all migrations in order to find the effective CHECK constraint.

    Handles DROP CONSTRAINT / ADD CONSTRAINT pairs correctly: a later
    migration that drops and re-adds the constraint replaces the earlier
    value list.
    """
    effective: set[str] = set()
    for migration in sorted(MIGRATIONS.glob("*.sql")):
        text = migration.read_text()
        # Check for DROP CONSTRAINT
        if re.search(
            rf"DROP\s+CONSTRAINT\s+(?:IF\s+EXISTS\s+)?{re.escape(constraint_name)}",
            text,
            re.IGNORECASE,
        ):
            effective = set()
        # Check for ADD CONSTRAINT with a value list
        add_match = re.search(
            rf"ADD\s+CONSTRAINT\s+{re.escape(constraint_name)}\s*\n?\s*CHECK\s*\((?:platform\s*=\s*ANY\s*\(\s*ARRAY\s*\[|platform\s+IN\s*\()\s*(.*?)\s*[)\]]",
            text,
            re.IGNORECASE | re.DOTALL,
        )
        if add_match:
            values = re.findall(r"'([^']+)'", add_match.group(1))
            effective = set(values)
    return effective


class PlatformVocabularyContract(unittest.TestCase):
    def test_synced_platforms_exist(self):
        """SYNCED_PLATFORMS must be defined and non-empty."""
        source = SYNC_SOURCE.read_text()
        platforms = parse_synced_platforms(source)
        self.assertTrue(
            len(platforms) > 0,
            "SYNCED_PLATFORMS must contain at least one platform",
        )

    def test_synced_platforms_in_fanbase_connections(self):
        """Every SYNCED_PLATFORMS entry must be in the effective
        fanbase_connections_platform_check constraint. Without this,
        the worker's find_due_connections query fails because the
        connection row violates the constraint."""
        source = SYNC_SOURCE.read_text()
        synced = parse_synced_platforms(source)
        allowed = parse_effective_constraint("fanbase_connections_platform_check")
        missing = synced - allowed
        self.assertEqual(
            missing,
            set(),
            f"Platforms in SYNCED_PLATFORMS but missing from "
            f"fanbase_connections_platform_check: {missing}. "
            f"Add them to the next migration that updates this constraint.",
        )

    def test_metric_platforms_in_growth_series(self):
        """Every platform string passed to record_metric_point() must be
        in the effective viryaos_growth_metric_series_platform_check
        constraint. Without this, recording a metric point fails with
        a constraint violation at runtime."""
        source = SYNC_SOURCE.read_text()
        metric_platforms = parse_metric_platforms(source)
        allowed = parse_effective_constraint(
            "viryaos_growth_metric_series_platform_check"
        )
        missing = metric_platforms - allowed
        self.assertEqual(
            missing,
            set(),
            f"Platforms passed to record_metric_point but missing from "
            f"viryaos_growth_metric_series_platform_check: {missing}. "
            f"Add them to the next migration that updates this constraint.",
        )

    def test_sync_notify_trigger_keeps_no_platform_allowlist(self):
        """The NOTIFY trigger must not filter on platform.

        It used to: `IF NEW.platform IN ('youtube', 'spotify', 'reddit')`, a
        third copy of the platform vocabulary written in SQL. The connectable
        surface grew to seventeen platforms and the worker's SYNCED_PLATFORMS to
        fourteen while that list stayed at three, so connecting SoundCloud or
        TikTok raised no NOTIFY and the first sync waited for the worker's next
        scheduled wake.

        The fix was to delete the list, not to sync it — the worker's lease
        query already filters on SYNCED_PLATFORMS, so a NOTIFY for a platform it
        does not poll costs one wakeup that finds nothing. Over-notifying is
        cheap and self-correcting; under-notifying is silent. This test fails if
        an allowlist reappears.
        """
        body = parse_effective_trigger_body()
        allowlist = re.search(
            r"NEW\.platform\s*(?:IN\s*\(|=\s*ANY)",
            body,
            re.IGNORECASE,
        )
        self.assertIsNone(
            allowlist,
            "notify_growth_metric_sync() filters on NEW.platform again. That "
            "list has to track SYNCED_PLATFORMS by hand and will drift. Let the "
            "trigger notify for every connected platform and let the worker "
            "decide what it polls.",
        )

    def test_metric_platforms_are_non_empty(self):
        """Sanity: record_metric_point must be called at least once."""
        source = SYNC_SOURCE.read_text()
        metric_platforms = parse_metric_platforms(source)
        self.assertTrue(
            len(metric_platforms) > 0,
            "No record_metric_point calls found — the parser may be broken "
            "or the worker has no metric recording.",
        )


if __name__ == "__main__":
    unittest.main()
