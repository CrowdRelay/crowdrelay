#!/usr/bin/env python3
"""Tenant settings v1 contract: per-tenant data instead of a fork.

The first extraction moves the member-site URL, its area path and the
synesthesia campaign slug behind crowdrelay-infra::tenant_settings. Two
invariants matter more than the feature itself:

- ZERO REGRESSION: the shipped defaults must byte-equal the constants the code
  used before the extraction, so an empty settings table changes nothing.
- NO WRITE SQL outside the infra repository, as with every newer surface.
"""
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

INFRA = ROOT / "crates/crowdrelay-infra/src/tenant_settings.rs"
MIGRATION = ROOT / "migrations/0111_tenant_settings.sql"
WORKER = ROOT / "crates/crowdrelay-worker/src/reminders.rs"


class TenantSettingsContract(unittest.TestCase):
    def test_defaults_byte_match_the_extracted_constants(self):
        source = INFRA.read_text()
        self.assertIn('DEFAULT_MEMBER_SITE_BASE_URL: &str = "https://virya.music"', source)
        self.assertIn('DEFAULT_MEMBER_AREA_PATH: &str = "pl/latarnik"', source)
        self.assertIn(
            'DEFAULT_SYNESTHESIA_CAMPAIGN_SLUG: &str = "virya-synesthesia-album-v1"',
            source,
        )
        # The releases URL keeps its anchor exactly as before extraction.
        self.assertIn('"https://virya.music/pl/latarnik/#wydania"', source)

    def test_invite_url_keeps_the_old_locale_branching_and_shape(self):
        source = INFRA.read_text()
        self.assertIn('if locale.starts_with("pl")', source)
        # No added slash before the query: byte parity with the old format!().
        self.assertIn('"{}/{}?invite={}"', source)

    def test_cache_is_ttl_bounded_and_invalidated_on_write(self):
        source = INFRA.read_text()
        self.assertRegex(source, r"CACHE_TTL: Duration = Duration::from_secs\(\d+\)")
        self.assertIn("cache.remove(&workspace_id)", source.split("pub async fn set_setting")[1])

    def test_settings_table_is_scoped_and_bounded(self):
        migration = MIGRATION.read_text()
        self.assertIn("PRIMARY KEY (workspace_id, key)", migration)
        self.assertIn("REFERENCES workspaces(id) ON DELETE CASCADE", migration)
        self.assertRegex(migration, r"char_length\(value\) <= \d+")

    def test_call_sites_no_longer_carry_the_hardcoded_release_url(self):
        worker = WORKER.read_text()
        self.assertNotIn(
            'RELEASE_MEMBER_URL: &str = "https://virya.music',
            worker,
            "the constant moved to tenant_settings; re-hardcoding it is a regression",
        )
        self.assertIn("brand_settings(", worker)


if __name__ == "__main__":
    unittest.main()
