#!/usr/bin/env python3
"""The operator-attention aggregate must equal its individual endpoints.

The Control Plane renders its Operator Attention page from the single
`/v1/control-plane/ops/attention` call instead of fanning out to the five
endpoints it aggregates. That is only safe while the aggregate returns the
same rows those endpoints would: same filters, same ordering, same bounds,
and the same lazily-seeded feature flags. Drift here is invisible in the UI --
the page simply shows fewer findings or flags than the dedicated views do.
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def read(relative: str) -> str:
    return (ROOT / relative).read_text(encoding="utf-8")


class OperatorAttentionAggregateContract(unittest.TestCase):
    def setUp(self):
        self.attention = read("crates/crowdrelay-api/src/ops/attention.rs")
        self.ecosystem = read("crates/crowdrelay-api/src/ecosystem.rs")
        self.handlers = read("crates/crowdrelay-api/src/ops/handlers.rs")

    def test_aggregate_reuses_the_canonical_summary_loader(self):
        # Not a re-implementation: both paths must call the same function.
        self.assertIn("load_summary(&state.ops)", self.attention)
        self.assertIn("load_summary(&state.ops)", self.handlers)

    def test_aggregate_seeds_lazy_flag_defaults_like_the_overview(self):
        self.assertIn("ensure_default_flags(state)", self.attention)
        self.assertIn("ensure_default_flags(&state)", self.ecosystem)

    def test_aggregate_reports_the_shared_snapshot_schema(self):
        self.assertIn(
            "schema_version: crate::ecosystem::SHOW_SNAPSHOT_SCHEMA",
            self.attention,
        )
        # A literal here silently drifts the moment the constant is bumped.
        self.assertNotIn("schema_version: 1,", self.attention)

    def test_aggregate_keeps_the_same_bounds_as_the_list_endpoints(self):
        # Control Plane previously requested limit=50 explicitly; the aggregate
        # must not quietly return a different page size.
        self.assertEqual(len(re.findall(r"LIMIT 50", self.attention)), 3)

    def test_aggregate_filters_open_findings_only(self):
        findings = self.attention.split("async fn load_open_findings", 1)[1]
        self.assertIn("resolved_at IS NULL", findings)
        self.assertIn("ORDER BY created_at DESC, id DESC", findings)

    def test_aggregate_selects_dead_queue_items_only(self):
        outbox = self.attention.split("async fn load_dead_outbox", 1)[1].split(
            "async fn ", 1
        )[0]
        self.assertIn("status = 'dead'", outbox)
        self.assertIn("ORDER BY created_at DESC, id DESC", outbox)
        deliveries = self.attention.split("async fn load_dead_deliveries", 1)[1].split(
            "async fn ", 1
        )[0]
        self.assertIn("delivery.status = 'dead'", deliveries)
        self.assertIn("ORDER BY delivery.created_at DESC, delivery.id DESC", deliveries)

    def test_aggregate_bounds_every_section_with_a_timeout(self):
        # One slow section must not hang the whole operator page.
        self.assertEqual(len(re.findall(r"run_with_timeout\(", self.attention)), 5)


if __name__ == "__main__":
    unittest.main()
