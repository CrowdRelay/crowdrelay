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
        # must not quietly return a different page size. Asserted per loader
        # rather than as a total so a new list section cannot pass by borrowing
        # another section's bound.
        for loader in (
            "load_alerts",
            "load_dead_outbox",
            "load_dead_deliveries",
            "load_dead_push",
            "load_open_findings",
        ):
            body = self.attention.split(f"async fn {loader}", 1)[1].split("async fn ", 1)[0]
            self.assertIn("LIMIT 50", body, f"{loader} must stay bounded")

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
        # One slow section must not hang the whole operator page. Named instead
        # of counted: a new section has to be added here deliberately.
        #
        # The wrapper used to be `run_with_timeout`. It is `run_limited` now,
        # which still applies the same timeout and additionally holds a
        # semaphore permit — eleven reads against a pool of eight would
        # otherwise take every connection this API has and make every other
        # request queue behind one operator refresh. This asserts the bound is
        # present, not which helper spells it, so both names count.
        sections = re.findall(
            r"let (\w+) =\s*run_(?:with_timeout|limited)\(", self.attention
        )
        self.assertEqual(
            sorted(sections),
            [
                "alerts",
                "blocked_communities",
                "brain",
                "dead_deliveries",
                "dead_outbox",
                "dead_push",
                "ecosystem",
                "findings",
                "needs_you",
                "summary",
                "unpublished_drafts",
            ],
        )

    def test_aggregate_sections_share_one_connection_limiter(self):
        # The timeout bounds how long a section may take; the shared budget
        # bounds how many hold a connection at once. Without it the page asks
        # for more connections than the pool holds, which is a self-inflicted
        # outage rather than a slow page, so the limiter is asserted rather
        # than left to review.
        #
        # The budget is `AppState.read_budget` — one semaphore shared by every
        # control-plane read, not a semaphore built per request. A per-request
        # limiter let the page take the entire pool when nine endpoints fanned
        # out at once (measured: 10 of 10 connections for one page load), so a
        # request-local `Semaphore::new` here would be a regression.
        self.assertIn("let budget = &state.read_budget;", self.attention)
        self.assertNotIn(
            "Semaphore::new(",
            self.attention,
            "the bound is the shared read budget, not a per-request semaphore",
        )
        for section in re.findall(r"let \w+ =\s*run_limited\(\s*(\w+)", self.attention):
            self.assertEqual(section, "budget")
        # The ecosystem arm pays its leaves instead of holding one permit for
        # the whole arm — it fans out five queries inside the join, and the
        # permit unit is the in-flight query.
        body = self.attention.split("async fn load_attention_ecosystem", 1)[1].split(
            "async fn ", 1
        )[0]
        self.assertGreaterEqual(
            body.count("hold(budget,"),
            5,
            "each in-flight query inside the ecosystem join pays one permit",
        )

    def test_the_limiter_budget_is_read_from_the_pool(self):
        """A budget that reasons about the pool has to read the pool.

        This was `const OPS_FAN_OUT_LIMIT: usize = 4` with a comment asserting
        "the pool is eight". Four numbers disagreed about the pool: the code
        default is 20, `.env.example` says 10, `deploy/env.production.example`
        says 5, and the comment said 8 — so "half the pool" was true of none of
        them. At five, one page load took 80% of the pool, which is the
        starvation the limiter exists to prevent.
        """
        fan_out = (
            ROOT / "crates/crowdrelay-api/src/ops/fan_out.rs"
        ).read_text()
        self.assertIn(
            "fn new(pool: &sqlx::PgPool)",
            fan_out,
            "the read budget must take the pool, not a constant",
        )
        self.assertIn(
            "pool.options().get_max_connections()",
            fan_out,
            "read the configured pool size rather than restating it",
        )
        self.assertNotRegex(
            fan_out,
            r"const OPS_FAN_OUT_LIMIT|const CONTROL_PLANE_READ",
            "a hardcoded budget cannot stay correct across four different "
            "configured pool sizes",
        )

    def test_the_budget_is_shared_by_the_pages_other_arms(self):
        """The budget only bounds what acquires from it.

        One instance lives on `AppState`, built from the real pool; the page's
        other arms — the autopilot read helper and the ecosystem overview's
        join — pay the same budget, otherwise an endpoint could fan out
        unbounded beside the nine that are bounded.
        """
        lib = read("crates/crowdrelay-api/src/lib.rs")
        self.assertIn("read_budget: ops::ControlPlaneReadBudget", lib)
        self.assertIn("ControlPlaneReadBudget::new(&database)", lib)
        autopilot = read("crates/crowdrelay-api/src/autopilot.rs")
        self.assertIn("crate::ops::budgeted(", autopilot)
        self.assertIn("&state.read_budget", autopilot)
        self.assertIn("let budget = &state.read_budget;", self.ecosystem)
        self.assertIn("crate::ops::hold(budget,", self.ecosystem)


if __name__ == "__main__":
    unittest.main()
