#!/usr/bin/env python3
"""Label Portfolio v1 contract: one tenant operates a roster.

The commercial core of multi-artist operations is the consent edge: fans never
leave their home workspace, beneficiary messages travel through the owner's
own outbox, and every edge carries purpose, monthly cap and cooldown. These
pins keep that model intact:

- Lifecycle transitions live once in the domain; revocation is terminal.
- All write statements stay in the infra repository.
- The delivery ledger dedupes consent+fan+campaign and carries the columns the
  cap and cooldown need; no email column exists on the ledger.
- Amplification outbox rows are always written into the OWNER workspace.
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

DOMAIN = ROOT / "crates/crowdrelay-domain/src/portfolio.rs"
INFRA = ROOT / "crates/crowdrelay-infra/src/portfolio.rs"
API = ROOT / "crates/crowdrelay-api/src/portfolio.rs"
MIGRATION = ROOT / "migrations/0110_label_portfolio_mode.sql"

WRITE_SQL = re.compile(r"\b(INSERT\s+INTO|UPDATE\s+\w+|DELETE\s+FROM)\b", re.IGNORECASE)


class LabelPortfolioContract(unittest.TestCase):
    def test_domain_owns_consent_lifecycle(self):
        source = DOMAIN.read_text()
        self.assertIn("ALLOWED_DECISIONS", source)
        table = source[source.index("pub const ALLOWED_DECISIONS"):]
        table = table[:table.index("];")]
        # Revocation is terminal: nothing resurrects a revoked edge.
        self.assertNotIn("(ConsentStatus::Revoked, ConsentStatus::Proposed)", table)
        self.assertNotIn("(ConsentStatus::Revoked, ConsentStatus::Active)", table)
        self.assertIn("(ConsentStatus::Paused, ConsentStatus::Active)", table)

    def test_delivery_policy_binds_cap_and_status(self):
        source = DOMAIN.read_text()
        self.assertIn("fn delivery_allowed(", source)
        self.assertIn("status != ConsentStatus::Active", source)
        self.assertIn("max_campaigns_per_month", source)

    def test_http_layer_carries_no_write_sql(self):
        writes = [m.group(0) for m in WRITE_SQL.finditer(API.read_text())]
        self.assertEqual(writes, [], f"write SQL leaked into the HTTP layer: {writes}")

    def test_ledger_shape_supports_cap_cooldown_and_privacy(self):
        migration = MIGRATION.read_text()
        self.assertRegex(
            migration,
            r"UNIQUE \(consent_id, fan_id, campaign_reference\)",
        )
        self.assertIn("delivered_at timestamptz NOT NULL DEFAULT now()", migration)
        # No address ever lands on the ledger: reach is counted, not copied.
        self.assertNotIn("normalized_email", migration.split("amplification_deliveries")[2])
        # Fans cannot amplify themselves.
        self.assertIn("CHECK (from_workspace_id <> to_workspace_id)", migration)

    def test_amplification_enqueues_into_the_owner_workspace(self):
        infra = INFRA.read_text()
        head = infra[infra.index("INSERT INTO outbox_events"):]
        head = head[:head.index("RETURNING")]
        self.assertIn(
            "edge.from_workspace_id", head,
            "amplification messages must leave through the owner's own outbox",
        )

    def test_case_study_export_is_pure_read_model(self):
        source = API.read_text()
        self.assertIn("pub async fn export_case_study", source)
        # A sales artifact must not leak identities either.
        head = source[source.index("pub async fn export_case_study"):]
        self.assertNotIn("normalized_email", head)

    def test_admin_surface_is_exposed_and_documented(self):
        api_module = API.read_text()
        self.assertIn("/v1/admin/portfolio/amplification", api_module)
        self.assertIn("pub(super) fn admin_routes()", api_module)
        routing = (ROOT / "crates/crowdrelay-api/src/routing.rs").read_text()
        self.assertIn(".merge(portfolio::admin_routes())", routing)
        openapi = (ROOT / "openapi/openapi.yaml").read_text()
        self.assertIn("/admin/portfolio/amplification:", openapi)
        self.assertIn("operationId: runPortfolioAmplificationCampaign", openapi)


if __name__ == "__main__":
    unittest.main()
