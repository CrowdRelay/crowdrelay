#!/usr/bin/env python3
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
API = (ROOT / "crates/crowdrelay-api/src/area.rs").read_text()
ROUTER = (ROOT / "crates/crowdrelay-api/src/lib.rs").read_text()
MIGRATION = (ROOT / "migrations/0029_area_game_backend.sql").read_text()

EXPECTED_DROPS = [
    "wro-001",
    "poz-002",
    "gdn-003",
    "waw-004",
    "ktw-005",
    "krk-006",
    "ldz-007",
    "szc-008",
    "lub-009",
    "rze-010",
    "bia-011",
    "tor-012",
]


class AreaGameContract(unittest.TestCase):
    def test_migration_seeds_exactly_the_public_catalogue_for_virya(self):
        seeded = re.findall(r"\('([a-z]{3}-\d{3})','\d{3}'", MIGRATION)
        self.assertEqual(seeded, EXPECTED_DROPS)
        self.assertIn("WHERE workspace.slug = 'virya'", MIGRATION)
        self.assertIn("seeded <> 12", MIGRATION)
        self.assertIn("2027-12-31T23:59:59+01:00", MIGRATION)
        self.assertNotIn("2028-12-31", MIGRATION)
        self.assertIn("seed.collectible_edition, 'Yanus', false", MIGRATION)
        self.assertIn("seed.approximate_lat, seed.approximate_lng, NULL, NULL", MIGRATION)
        self.assertNotRegex(
            MIGRATION,
            r"seed\.approximate_lat, seed\.approximate_lng, seed\.exact_lat",
        )

    def test_claims_are_capacity_limited_and_auditable(self):
        self.assertIn("UNIQUE (workspace_id, drop_id, edition_number)", MIGRATION)
        self.assertIn("claim_source IN ('gps', 'legacy_import')", MIGRATION)
        self.assertIn("radius_meters BETWEEN 25 AND 500", MIGRATION)
        self.assertIn("max_claims BETWEEN 1 AND 500", MIGRATION)
        self.assertIn("FOR UPDATE", API)
        self.assertIn("next_edition_number", API)
        self.assertIn("ON CONFLICT (workspace_id, player_id, drop_id) DO NOTHING", API)

    def test_private_geometry_never_enters_public_dtos(self):
        public_drop = re.search(
            r"struct PublicDrop \{(?P<body>.*?)\n\}", API, re.DOTALL
        )
        self.assertIsNotNone(public_drop)
        body = public_drop.group("body")
        self.assertNotIn("exact_lat", body)
        self.assertNotIn("exact_lng", body)
        self.assertNotIn("radius_meters", body)
        self.assertIn("approximate_lat", body)
        self.assertIn("approximate_lng", body)

    def test_api_has_one_canonical_public_and_private_contract(self):
        for route in [
            '"/v1/public/area/drops"',
            '"/v1/me/area"',
            '"/v1/me/area/challenge"',
            '"/v1/me/area/claim"',
            '"/v1/internal/area/players"',
            '"/v1/internal/area/players/{player_id}/claims/import"',
        ]:
            self.assertIn(route, ROUTER)
        self.assertIn("fan_session_from_headers", API)
        self.assertIn("commerce_authorized", API)
        self.assertNotIn("admin_authorized", API)

    def test_mutations_require_uuid_idempotency_keys(self):
        self.assertIn("Uuid::parse_str(value).is_ok()", API)
        self.assertGreaterEqual(API.count("valid_idempotency_key(&headers)"), 4)

    def test_legacy_import_preserves_available_metadata(self):
        self.assertIn("claimed_at: Option<OffsetDateTime>", API)
        self.assertIn("edition_number: Option<u32>", API)
        self.assertIn("'legacy_import'", API)
        self.assertIn("fallback_claimed_at", API)


if __name__ == "__main__":
    unittest.main()
