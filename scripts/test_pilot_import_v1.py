#!/usr/bin/env python3
"""Pilot import v1 contract: day-one value without consent shortcuts.

The pilot offer promises an existing mailing list works from day one. The
consent model answers how: imported addresses land as `pending` and receive
the SAME double-opt-in email the signup flow uses. Layering follows the house
rule — HTTP validates and maps, all statements live in the infra repository.

Pins:
- imports may only create `pending` fans — never `active`;
- suppressed/unsubscribed addresses are skipped, not resurrected;
- confirmation goes out through the canonical `fan.confirmation_requested`
  event with a real token row, inside the resend cooldown;
- the batch writes one audit row naming the source;
- zero write SQL in the HTTP layer (api-sql ratchet companion).
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

INFRA = ROOT / "crates/crowdrelay-infra/src/fan_import.rs"
API = ROOT / "crates/crowdrelay-api/src/fan_lifecycle.rs"
ROUTING = ROOT / "crates/crowdrelay-api/src/portfolio.rs"

WRITE_SQL = re.compile(r"\b(INSERT\s+INTO|UPDATE\s+\w+|DELETE\s+FROM)\b", re.IGNORECASE)


class PilotImportContract(unittest.TestCase):
    def test_repository_creates_pending_only(self):
        source = INFRA.read_text()
        self.assertIn("'pending'", source)
        self.assertIn('"active" => {', source)
        self.assertIn("already_active += 1", source)
        # No code path writes an active status on import.
        self.assertNotIn("'active')", source.replace("VALUES ($1, $2, $3, $4, 'pending')", ""))

    def test_suppressed_never_resurrected(self):
        source = INFRA.read_text()
        self.assertIn('"unsubscribed" | "suppressed"', source)
        self.assertIn("skipped_suppressed", source)

    def test_confirmation_reuses_the_canonical_event_and_real_token(self):
        source = INFRA.read_text()
        self.assertIn("fan.confirmation_requested", source)
        self.assertIn("digest(material.token, 'sha256')", source)
        # The raw token travels only through the outbox payload.
        self.assertIn('"confirmation_token": raw_token', source)

    def test_resend_cooldown_applies(self):
        source = INFRA.read_text()
        self.assertIn("resend_cooldown_seconds", source)
        self.assertIn("cooldown_skipped", source)

    def test_batch_is_audited_with_source(self):
        source = INFRA.read_text()
        self.assertIn("'fans.imported'", source)
        self.assertIn('"source": source.trim()', source)

    def test_http_layer_carries_no_write_sql(self):
        api_source = (ROOT / "crates/crowdrelay-api/src/fan_lifecycle.rs").read_text()
        import_body = api_source.split("pub async fn import_fans_admin", 1)[1]
        writes = [m.group(0) for m in WRITE_SQL.finditer(import_body)]
        self.assertEqual(writes, [], f"write SQL leaked into the HTTP layer: {writes}")

    def test_route_mounted_under_portfolio_admin(self):
        routing = ROUTING.read_text()
        self.assertIn("/v1/admin/portfolio/import-fans", routing)
        openapi = (ROOT / "openapi/openapi.yaml").read_text()
        self.assertIn("operationId: importPortfolioFans", openapi)


if __name__ == "__main__":
    unittest.main()
