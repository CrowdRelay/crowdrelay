#!/usr/bin/env python3
"""Audience Graph v1 contract: prospecting data the brain can act on.

Outreach Supply asks for discovery sweeps; this schema is the supply. The pins
below keep the graph a first-class pipeline instead of an ad-hoc table:

- All write statements live in the infra repository; the HTTP layer maps
  protocol only (api-sql ratchet would catch a regression, but the boundary is
  also asserted here explicitly per module).
- Stage transitions are defined once in the domain; the database CHECK is a
  last-resort net that must stay narrower than the domain's move set.
- A refusal reopens only through research: there is no declined -> contacted.
- Every place dedupes per workspace on (platform, url), and every place's own
  rules carry a bounded cooldown, so enthusiasm cannot spam a community.
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

DOMAIN = ROOT / "crates/crowdrelay-domain/src/audience_graph.rs"
INFRA = ROOT / "crates/crowdrelay-infra/src/audience_graph.rs"
API = ROOT / "crates/crowdrelay-api/src/audience_graph.rs"
WORKER = ROOT / "crates/crowdrelay-worker/src/audience_graph.rs"
MIGRATION = ROOT / "migrations/0109_audience_graph.sql"

WRITE_SQL = re.compile(
    r"\b(INSERT\s+INTO|UPDATE\s+\w+|DELETE\s+FROM)\b", re.IGNORECASE
)


class AudienceGraphContract(unittest.TestCase):
    def test_domain_owns_the_transition_set(self):
        source = DOMAIN.read_text()
        self.assertIn("ALLOWED_TRANSITIONS", source)
        table = source[source.index("pub const ALLOWED_TRANSITIONS"):]
        table = table[:table.index("];")]
        # A refusal never jumps back into contact.
        self.assertNotIn(
            "(OutreachStage::Declined, OutreachStage::Contacted)", table,
            "declined places must reopen through research only",
        )
        self.assertIn(
            "(OutreachStage::Declined, OutreachStage::Researched)", table
        )
        # Discovery is unvetted: no direct pitch from it.
        self.assertNotIn(
            "(OutreachStage::Discovered, OutreachStage::Contacted)", table
        )

    def test_http_layer_carries_no_write_sql(self):
        source = API.read_text()
        writes = [
            match.group(0)
            for match in WRITE_SQL.finditer(source)
            if "discovery_" in source[match.start():match.start() + 80]
            or True  # any write statement at all is a regression here
        ]
        self.assertEqual(writes, [], f"write SQL leaked into the HTTP layer: {writes}")

    def test_repository_scopes_every_read_by_workspace(self):
        source = INFRA.read_text()
        for signature in (
            "pub async fn list_places",
            "pub async fn place_detail",
            "pub async fn attach_rules",
            "pub async fn advance_outreach(",
        ):
            head = source[source.index(signature):]
            body = head[:head.index("pub async fn", 10) if "pub async fn" in head[10:] else len(head)]
            self.assertIn(
                "workspace_id", body[:600],
                f"{signature} must take the workspace scope",
            )

    def test_pipeline_rows_are_unique_per_place(self):
        migration = MIGRATION.read_text()
        self.assertIn("UNIQUE (workspace_id, platform, url)", migration)
        self.assertRegex(
            migration, r"place_id uuid NOT NULL UNIQUE REFERENCES discovery_places"
        )
        self.assertRegex(migration, r"cooldown_days smallint NOT NULL DEFAULT \d+")

    def test_worker_decay_pass_exists_and_is_wired(self):
        worker = WORKER.read_text()
        self.assertIn("decay_dormant", worker)
        main = (ROOT / "crates/crowdrelay-worker/src/main.rs").read_text()
        self.assertIn("AudienceGraphSweeper", main)
        self.assertIn("audience_graph_sweeper.run(", main)

    def test_admin_surface_is_exposed_and_documented(self):
        api_module = API.read_text()
        self.assertIn("/v1/admin/audience-graph/places", api_module)
        self.assertIn("pub(super) fn admin_routes()", api_module)
        routing = (ROOT / "crates/crowdrelay-api/src/routing.rs").read_text()
        self.assertIn(".merge(audience_graph::admin_routes())", routing)
        openapi = (ROOT / "openapi/openapi.yaml").read_text()
        self.assertIn("/admin/audience-graph/places:", openapi)
        self.assertIn("operationId: importAudienceGraphScan", openapi)


if __name__ == "__main__":
    unittest.main()
