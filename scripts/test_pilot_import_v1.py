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
        """An import may admit a fan as `pending` and nothing else.

        Asserted against the status match arms rather than one statement's
        exact text: the loop became a set-based batch, so the insert is now an
        `INSERT ... SELECT FROM unnest(...)`. The property is unchanged and is
        what the arms encode -- `active` is counted, never written.
        """
        source = INFRA.read_text()
        self.assertIn("'pending'", source)
        self.assertIn('Some("active") => counts.already_active += 1', source)
        # No code path writes an active status on import. The only status
        # literal an INSERT may carry is 'pending'.
        inserted_statuses = re.findall(
            r"INSERT INTO fans\b.*?;", source, re.IGNORECASE | re.DOTALL
        )
        self.assertTrue(inserted_statuses, "the fans insert was not found; the parser is wrong")
        for statement in inserted_statuses:
            self.assertNotIn(
                "'active'",
                statement,
                "an import must never insert a fan as active",
            )

    def test_suppressed_never_resurrected(self):
        source = INFRA.read_text()
        self.assertIn('"unsubscribed" | "suppressed"', source)
        self.assertIn("skipped_suppressed", source)

    def test_confirmation_reuses_the_canonical_event_and_real_token(self):
        """Only the hash is stored, and the raw token leaves only by outbox.

        The batch rewrite mints one token per recipient instead of one per
        loop iteration, so the payload now reads the token out of a map. What
        must not change: the database holds `digest(...)` and the raw value
        appears in exactly one place, the outbox payload.
        """
        source = INFRA.read_text()
        self.assertIn("fan.confirmation_requested", source)
        self.assertIn("digest(material.token, 'sha256')", source)
        self.assertIn('"confirmation_token": token_of.get(fan_id).copied()', source)
        # One mention, in the payload. A second would be a log line or a
        # return value carrying a live credential.
        self.assertEqual(
            source.count("confirmation_token"),
            1,
            "the raw confirmation token may appear only in the outbox payload",
        )

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
