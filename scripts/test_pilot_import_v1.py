#!/usr/bin/env python3
"""Pilot import v1 contract: day-one value without consent shortcuts.

The pilot offer promises an existing mailing list works from day one. The
consent model answers how: imported addresses land as `pending` and receive
the SAME double-opt-in email the signup flow uses. These pins keep the
shortcut-free behavior from eroding under delivery pressure:

- imports may only create `pending` fans — never `active`;
- suppressed/unsubscribed addresses are skipped, not resurrected;
- confirmation goes out through the canonical `fan.confirmation_requested`
  event with a real token row, inside the resend cooldown;
- the batch writes one audit row naming the source.
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SOURCE = (ROOT / "crates/crowdrelay-api/src/fan_lifecycle.rs").read_text()
IMPORT = SOURCE.split("pub async fn import_fans_admin", 1)[1]


class PilotImportContract(unittest.TestCase):
    def test_import_creates_pending_only(self):
        body = IMPORT[: IMPORT.index("counts.confirmation_resent +=")]
        self.assertIn("'pending'", body)
        self.assertNotIn("VALUES ($1, $2, $3, $4, 'active')", body)
        self.assertRegex(body, r'"active" => \{[^}]*already_active')

    def test_suppressed_never_resurrected(self):
        self.assertIn('"unsubscribed" | "suppressed"', IMPORT)
        self.assertIn("skipped_suppressed", IMPORT)

    def test_confirmation_reuses_the_canonical_event_and_real_token(self):
        self.assertIn("fan.confirmation_requested", IMPORT)
        self.assertIn("digest(material.token, 'sha256')", IMPORT)
        # The token itself travels only through the outbox payload.
        self.assertIn('"confirmation_token": raw_token', IMPORT)

    def test_batch_is_bounded_and_audited_with_source(self):
        self.assertRegex(IMPORT, r"MAX_IMPORT_ENTRIES: usize = \d+")
        self.assertIn("'fans.imported'", IMPORT)
        self.assertIn('"source": request.source.trim()', IMPORT)

    def test_route_mounted_under_portfolio_admin(self):
        routing = (ROOT / "crates/crowdrelay-api/src/portfolio.rs").read_text()
        self.assertIn("/v1/admin/portfolio/import-fans", routing)
        openapi = (ROOT / "openapi/openapi.yaml").read_text()
        self.assertIn("operationId: importPortfolioFans", openapi)


if __name__ == "__main__":
    unittest.main()
