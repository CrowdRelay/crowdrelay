#!/usr/bin/env python3
"""Catch capabilities the operator can see but cannot use — or cannot see at all.

The recurring defect in this system is not broken code — it is code that works
and is unreachable. Found so far, each one by bumping into it:

  - Release campaigns could be listed, launched and closed. `create` was
    registered on `/v1/admin` and never in the control-plane namespace, so the
    panel's own empty state pointed at a surface that did not exist.
  - Communities could be read by the brain and registered only via psql.
  - The beacon roster had six read endpoints and no writes at all, so the
    people who carry local growth could be watched and never changed.
  - 121 admin routes had no control-plane twin at all: the ticket sale, merch
    stock, fan rewards, the release plan, the conversion funnel, and two of the operator questions CLAUDE.md answers with an
    endpoint (`ops/cycles`, `ops/connections`). This gate skipped every one of
    them with the comment "not exposed at all — a different, deliberate
    choice". Nobody had made that choice; the skip made it for them.

Two rules, both mechanical:

1. **Every `/v1/admin` route is either reachable from the control plane or
   named in `EXPECTED_ADMIN_ONLY` with the reason.** An entry there is a
   decision someone made on purpose. An absence is a gap nobody noticed. A
   route served on both surfaces under different names is an `ALIAS`, and the
   alias must point at a control-plane route that exists.

2. **If the control plane can GET a path, it also exposes that path's write
   verbs, or `EXPECTED_READ_ONLY` says why not.** A panel that shows state and
   cannot change it is the original defect.

Both lists are checked for dead entries in both directions: an exemption for a
route that no longer exists, or for one the control plane now reaches, hides
the next gap.
"""
from __future__ import annotations

import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
API = ROOT / "crates/crowdrelay-api/src"

# Every file that registers a route. `test_claude_md_counts_v1.py` fails when a
# file outside this list starts registering one, so it cannot silently shrink.
ROUTE_FILES = [
    "routing.rs",
    "control_plane.rs",
    "control_plane_operator.rs",
    "ops_routes.rs",
    "area_admin.rs",
    "synesthesia.rs",
    "portfolio.rs",
    "audience_graph.rs",
    "community_intelligence_routes.rs",
    "routing/growth.rs",
    "content_engine.rs",
    "media.rs",
    "team_approvals.rs",
]

# Admin routes the control plane deliberately cannot reach, keyed by
# `METHOD tail` (the path after `/v1/admin/`), with the reason. Adding here is
# how you record a decision; leaving a route out of both this list and the
# control plane is how the check tells you nobody made one.
EXPECTED_ADMIN_ONLY: dict[str, str] = {
    # Financial records. Finalising ticket-sale accounting and exporting the
    # documents re-check the admin key inside every handler.
    "GET accounting/profile": "financial records stay on the admin credential",
    "POST accounting/profile": "financial records stay on the admin credential",
    "GET accounting/ticket-sales/preview": "financial records stay on the admin credential",
    "POST accounting/ticket-sales/finalize": "financial records stay on the admin credential",
    "GET accounting/invoice-requests": "financial records stay on the admin credential",
    "GET accounting/documents/{}/csv": "financial records stay on the admin credential",
    # Credentials: a pass is admission, a pairing code mints a door-staff
    # device session. Neither is minted by the console.
    "POST admission/passes": "admission passes are minted only by the box office",
    "POST admission/passes/{}/revoke": "admission passes are managed only by the box office",
    "POST staff/pairing-codes": "door-staff device credentials are minted only by admin",
    "GET staff/sessions": "door-staff device credentials are managed only by admin",
    "POST staff/sessions/{}/revoke": "door-staff device credentials are managed only by admin",
    # Permanent deletion stays behind the stronger credential.
    "POST reward-draws/{}/delete": "permanent deletion stays admin-only",
    # Worker and feed ingestion: written by n8n, sync workers and the ad
    # provider feed reporting what they observed, not by a person. The
    # operator's lever on each is a different, exposed route.
    "POST autopilot/growth-metrics/series": "metric ingestion from sync workers",
    "POST autopilot/growth-metrics/points": "metric ingestion from sync workers",
    "POST autopilot/market-signals/city": "market-signal ingestion from feeds",
    "POST autopilot/promotion-state": "ad-provider snapshot stamped by the feed",
    "POST autopilot/outreach-opportunities": "discovery-sweep output; the lever is candidate confirm",
    "POST autopilot/team-opportunities": "scored by the ingestion feed; the lever is team-opportunities/discover",
    "POST autopilot/experiments": "the experiment lifecycle belongs to the brain",
    "POST autopilot/experiments/{}/assign": "the experiment lifecycle belongs to the brain",
    "POST autopilot/experiments/observations": "measurement ingestion",
    "POST ecosystem/checklists/emit-due": "scheduler trigger for the worker",
    # Proof batches are the pipeline's internals; the operator's proof surface
    # is attestations, which are exposed.
    "GET proofs/batches": "proof pipeline internals; attestations are the operator surface",
    "POST proofs/audit-batches": "proof pipeline internals; attestations are the operator surface",
    # Organisations span workspaces. A tenant console is scoped to one
    # workspace and edits its own settings through tenant-settings, and one
    # act's token must not read its labelmates' audiences — every roster-plan
    # read says so at its handler.
    "GET roster-plan": "organisation-wide; one act's token must not read its labelmates",
    "POST roster-plan/support-slot-ask": "organisation-wide; one act's token must not read its labelmates",
    "GET roster-plan/overview": "organisation-wide; one act's token must not read its labelmates",
    "GET roster-plan/weekly-brief": "organisation-wide; one act's token must not read its labelmates",
    "GET roster-plan/source-roi": "organisation-wide; one act's token must not read its labelmates",
    "GET roster-plan/portfolio": "organisation-wide; one act's token must not read its labelmates",
    "GET roster-plan/act-report": "organisation-wide; one act's token must not read its labelmates",
    "GET roster-plan/release-calendar": "organisation-wide; one act's token must not read its labelmates",
    "GET roster-plan/catalogue-rotation": "organisation-wide; one act's token must not read its labelmates",
    "POST roster-plan/catalogue-rotation": "organisation-wide; one act's token must not read its labelmates",
    "GET roster-plan/counterparty-archive": "organisation-wide; one act's token must not read its labelmates",
    "POST portfolio/organization": "organisation creation is platform provisioning",
    "GET organizations/{}/settings": "cross-workspace; the tenant edits tenant-settings",
    "PUT organizations/{}/settings/{}": "cross-workspace; the tenant edits tenant-settings",
}

# Admin routes served on the control plane under a different path by the same
# handler. The value is the control-plane `METHOD tail` that serves it.
ALIASES: dict[str, str] = {
    "GET analytics/city-funnel": "GET audience/city-funnel",
    "GET analytics/city-venues": "GET audience/city-venues",
    "GET signal/overview": "GET ops/signal-overview",
    "GET ecosystem/reconciliation": "GET ecosystem/findings",
    # The console writes settings with the POST alias; see EXPECTED_READ_ONLY.
    "PUT tenant-settings/{}": "POST tenant-settings/{}",
}

# Paths the control plane reads and deliberately cannot write, with the reason.
EXPECTED_READ_ONLY: dict[str, str] = {
    # Written by discovery workers reporting what they found, not by an
    # operator. The operator's lever is promoting a candidate, which is a
    # different route and is exposed.
    "autopilot/beacon-network": "worker-reported discovery output",
    "autopilot/beacon-press-assets": "worker-reported press asset discovery",
    "autopilot/booking-discovery/candidates": "worker-reported sweep output",
    "autopilot/outreach/candidates": "worker-reported sweep output",
    # Segment definitions are a product decision that ships in code; creating
    # one at runtime would let a tenant define an audience the brain has no
    # model for.
    "audience/segments": "segment definitions ship with the product",
    # The tenant accepts POST for this and the control plane uses POST; the
    # PUT alias exists only for older clients.
    "tenant-settings/{key}": "control plane uses the POST alias",
    # Economics are computed from ticket and cost data. The PUT is a
    # back-office correction path that has to go through the tenant's own
    # admin surface with its stronger credential.
    "autopilot/tour-economics": "computed; corrections use the admin credential",
    # See EXPECTED_ADMIN_ONLY: the sale is set by the box office.
    "events/{slug}/ticketing": "sale configuration re-checks the admin key in the handler",
}

WRITE_VERBS = {"post", "put", "patch", "delete"}
PARAM = re.compile(r"\{[^}]+\}")


def routes(source: str) -> dict[str, set[str]]:
    """Every routed path in a router file, mapped to its HTTP verbs."""
    # Comments sit between a path and its verbs in several routes; left in,
    # they hide those verbs from the pattern below.
    source = re.sub(r"//[^\n]*", "", source)
    found: dict[str, set[str]] = {}
    pattern = (
        r'"(/v1/[^"]+)"\s*,\s*'
        r"((?:get|post|put|delete|patch)\([^)]*\)"
        r"(?:\s*\.\s*(?:layer|get|post|put|delete|patch)\([^()]*(?:\([^()]*\)[^()]*)*\))*)"
    )
    for match in re.finditer(pattern, source, re.S):
        verbs = set(re.findall(r"\b(get|post|put|delete|patch)\(", match.group(2)))
        found.setdefault(match.group(1), set()).update(verbs)
    return found


def all_routes() -> dict[str, set[str]]:
    found: dict[str, set[str]] = {}
    for name in ROUTE_FILES:
        for path, verbs in routes((API / name).read_text()).items():
            found.setdefault(path, set()).update(verbs)
    return found


def tail(path: str) -> str:
    for prefix in ("/v1/admin/", "/v1/control-plane/"):
        if path.startswith(prefix):
            return path[len(prefix) :]
    return path


def keys(prefix: str, table: dict[str, set[str]]) -> set[str]:
    """`METHOD tail` for every route under `prefix`, parameters by position."""
    return {
        f"{verb.upper()} {PARAM.sub('{}', tail(path))}"
        for path, verbs in table.items()
        if path.startswith(prefix)
        for verb in verbs
    }


class OperatorReachability(unittest.TestCase):
    def setUp(self) -> None:
        table = all_routes()
        self.admin_paths = {p: v for p, v in table.items() if p.startswith("/v1/admin/")}
        self.plane_paths = {
            tail(p): v for p, v in table.items() if p.startswith("/v1/control-plane/")
        }
        self.admin = keys("/v1/admin/", table)
        self.plane = keys("/v1/control-plane/", table)

    def test_the_parser_still_sees_the_routers(self) -> None:
        """A regex that matches nothing would make every other check vacuous."""
        self.assertGreater(len(self.admin), 200, "admin route parse collapsed")
        self.assertGreater(len(self.plane), 250, "control-plane route parse collapsed")
        # Chained verbs and route-local layers must both parse: these are
        # known to carry two verbs or a `.layer(...)` between them.
        self.assertIn("POST merch/catalog", self.plane)
        self.assertIn("GET merch/catalog", self.plane)
        self.assertIn("POST audience-graph/places/import", self.plane)

    def test_every_admin_route_is_reachable_or_decided(self) -> None:
        # A write whose path is in EXPECTED_READ_ONLY is already decided: the
        # read is exposed and the write deliberately is not.
        read_only = {PARAM.sub("{}", name) for name in EXPECTED_READ_ONLY}
        undecided = sorted(
            key
            for key in self.admin
            if key not in self.plane
            and key not in EXPECTED_ADMIN_ONLY
            and key not in ALIASES
            and key.split(" ", 1)[1] not in read_only
        )
        self.assertEqual(
            undecided,
            [],
            "these admin routes have no control-plane twin and no recorded "
            "decision:\n  "
            + "\n  ".join(undecided)
            + "\n\nExpose them in control_plane_operator.rs, or add them to "
            "EXPECTED_ADMIN_ONLY with the reason. Silence is the defect.",
        )

    def test_aliases_point_at_a_served_route(self) -> None:
        broken = sorted(
            f"{admin} -> {plane}"
            for admin, plane in ALIASES.items()
            if plane not in self.plane
        )
        self.assertEqual(broken, [], f"aliases name missing control-plane routes: {broken}")

    def test_the_decision_lists_have_no_dead_entries(self) -> None:
        """An exemption for a route that is gone, or now exposed, hides the next gap."""
        gone = sorted(
            key for key in {**EXPECTED_ADMIN_ONLY, **ALIASES} if key not in self.admin
        )
        self.assertEqual(gone, [], f"decisions name admin routes that no longer exist: {gone}")
        exposed = sorted(key for key in EXPECTED_ADMIN_ONLY if key in self.plane)
        self.assertEqual(
            exposed,
            [],
            f"EXPECTED_ADMIN_ONLY names routes the control plane now serves: {exposed}",
        )
        stale = [
            name
            for name in EXPECTED_READ_ONLY
            if name not in {tail(p) for p in self.admin_paths}
        ]
        self.assertEqual(
            stale, [], f"EXPECTED_READ_ONLY names routes that no longer exist: {stale}"
        )

    def test_readable_resources_are_also_writable(self) -> None:
        gaps: list[str] = []
        for path, verbs in sorted(self.admin_paths.items()):
            name = tail(path)
            exposed = self.plane_paths.get(name)
            if exposed is None:
                continue  # Covered by the reachability rule above.
            missing = (verbs & WRITE_VERBS) - exposed
            if missing and name not in EXPECTED_READ_ONLY:
                gaps.append(f"{path} exposes {sorted(exposed)} but not {sorted(missing)}")
        self.assertEqual(
            gaps,
            [],
            "the control plane can read these and not act on them:\n  "
            + "\n  ".join(gaps)
            + "\n\nEither expose the write verb, or add the path to "
            "EXPECTED_READ_ONLY with the reason. A panel that shows state and "
            "cannot change it is the defect this check exists to catch.",
        )


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    if result.wasSuccessful():
        table = all_routes()
        admin = keys("/v1/admin/", table)
        plane = keys("/v1/control-plane/", table)
        reachable = sum(1 for key in admin if key in plane)
        print(
            f"OPERATOR_REACHABILITY=PASS admin={len(admin)} exposed={reachable} "
            f"aliased={len(ALIASES)} admin_only={len(EXPECTED_ADMIN_ONLY)} "
            f"read_only_by_design={len(EXPECTED_READ_ONLY)}"
        )
    else:
        print("OPERATOR_REACHABILITY=FAIL")
        sys.exit(1)
