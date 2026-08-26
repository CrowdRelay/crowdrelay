#!/usr/bin/env python3
"""Fanbases v1 contract: audience blocks with swappable origins.

A fanbase is an addressable block campaigns target; its provider is data.
Pins:

- admission follows the consent model (pending-only creation, opt-outs skip);
- community/follower origins cannot yield PII — the domain marks them and the
  adapter counts such candidates as invalid instead of inventing identities;
- membership attribution dedupes per external id;
- writes stay in infra; HTTP maps protocol only.
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

DOMAIN = ROOT / "crates/crowdrelay-domain/src/fanbase.rs"
INFRA = ROOT / "crates/crowdrelay-infra/src/fanbase.rs"
API = ROOT / "crates/crowdrelay-api/src/fanbase.rs"

WRITE_SQL = re.compile(r"\b(INSERT\s+INTO|UPDATE\s+\w+|DELETE\s+FROM)\b")


class FanbasesContract(unittest.TestCase):
    def test_source_kinds_carry_capabilities(self):
        source = DOMAIN.read_text()
        self.assertIn("fn pii_capable", source)
        self.assertIn("fn oauth_native", source)
        # Community platforms are graph signals, never address sources.
        self.assertIn("!matches!(\n            self,\n            Self::BandsintownFollowers | Self::RedditCommunity\n        )", source)

    def test_admission_follows_consent_model(self):
        source = DOMAIN.read_text()
        self.assertIn("fn admission_for", source)
        for status in ('"pending"', '"active"', '"unsubscribed"', '"suppressed"'):
            self.assertIn(status, source)
        # Unknown statuses fail safe into skip.
        self.assertIn("Some(_) => AdmissionAction::SkipSuppressed", source)

    def test_ingestion_uses_canonical_confirmation_and_membership(self):
        infra = INFRA.read_text()
        self.assertIn("fan.confirmation_requested", infra)
        self.assertIn("ON CONFLICT (fanbase_id, external_id) DO UPDATE SET", infra)

    def test_http_layer_carries_no_write_sql(self):
        api = API.read_text()
        writes = [m.group(0) for m in WRITE_SQL.finditer(api)]
        self.assertEqual(writes, [], f"write SQL leaked into HTTP layer: {writes}")

    def test_routes_mounted_and_documented(self):
        routing = (ROOT / "crates/crowdrelay-api/src/routing.rs").read_text()
        self.assertIn("/v1/admin/fanbases", routing)
        openapi = (ROOT / "openapi/openapi.yaml").read_text()
        self.assertIn("operationId: ingestFanbaseCandidates", openapi)


if __name__ == "__main__":
    unittest.main()
