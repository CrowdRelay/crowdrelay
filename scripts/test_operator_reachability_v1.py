#!/usr/bin/env python3
"""Catch capabilities the operator can see but cannot use.

The recurring defect in this system is not broken code — it is code that works
and is unreachable. Found so far, each one by bumping into it:

  - Release campaigns could be listed, launched and closed. `create` was
    registered on `/v1/admin` and never in the control-plane namespace, so the
    panel's own empty state pointed at a surface that did not exist.
  - Communities could be read by the brain and registered only via psql.
  - The beacon roster had six read endpoints and no writes at all, so the
    people who carry local growth could be watched and never changed.

They share one shape: **a resource whose reads are exposed and whose writes are
not.** That is mechanically detectable, and this checks it.

The rule is not "every admin route must be in the control plane" — most should
not be. Ingestion fed by workers, staff device pairing, accounting exports are
all correctly admin-only. The rule is narrower and harder to argue with:

    if the control plane can GET a path, it must also expose that path's
    write verbs, or say in EXPECTED_READ_ONLY why not.

An entry in that list is a decision someone made on purpose. An absence is a
gap nobody noticed.
"""
from __future__ import annotations

import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ROUTING = ROOT / "crates/crowdrelay-api/src/routing.rs"
CONTROL_PLANE = ROOT / "crates/crowdrelay-api/src/control_plane.rs"

# Paths the control plane reads and deliberately cannot write, with the reason.
# Adding to this list is how you record a decision; leaving a path out is how
# the check tells you nobody made one.
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
}

WRITE_VERBS = {"post", "put", "patch", "delete"}


def routes(source: str) -> dict[str, set[str]]:
    """Every routed path in a router file, mapped to its HTTP verbs."""
    found: dict[str, set[str]] = {}
    pattern = (
        r'"(/v1/[^"]+)"\s*,\s*'
        r"((?:get|post|put|delete|patch)\([^)]*\)"
        r"(?:\s*\.\s*(?:get|post|put|delete|patch)\([^)]*\))*)"
    )
    for match in re.finditer(pattern, source, re.S):
        verbs = set(re.findall(r"\b(get|post|put|delete|patch)\(", match.group(2)))
        found.setdefault(match.group(1), set()).update(verbs)
    return found


def tail(path: str) -> str:
    for prefix in ("/v1/admin/", "/v1/control-plane/"):
        if path.startswith(prefix):
            return path[len(prefix) :]
    return path


class OperatorReachability(unittest.TestCase):
    def setUp(self) -> None:
        self.admin = {
            path: verbs
            for path, verbs in routes(ROUTING.read_text()).items()
            if path.startswith("/v1/admin/")
        }
        self.plane = {tail(p): v for p, v in routes(CONTROL_PLANE.read_text()).items()}

    def test_the_parser_still_sees_the_routers(self) -> None:
        """A regex that matches nothing would make every other check vacuous."""
        self.assertGreater(len(self.admin), 100, "admin route parse collapsed")
        self.assertGreater(len(self.plane), 50, "control-plane route parse collapsed")

    def test_readable_resources_are_also_writable(self) -> None:
        gaps: list[str] = []
        for path, verbs in sorted(self.admin.items()):
            name = tail(path)
            exposed = self.plane.get(name)
            if exposed is None:
                continue  # Not exposed at all — a different, deliberate choice.
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

    def test_the_exemption_list_has_no_dead_entries(self) -> None:
        """An exemption for a path that no longer exists hides the next gap."""
        stale = [
            name
            for name in EXPECTED_READ_ONLY
            if name not in {tail(p) for p in self.admin}
        ]
        self.assertEqual(
            stale, [], f"EXPECTED_READ_ONLY names routes that no longer exist: {stale}"
        )


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    if result.wasSuccessful():
        admin = {p: v for p, v in routes(ROUTING.read_text()).items() if p.startswith("/v1/admin/")}
        plane = {tail(p) for p in routes(CONTROL_PLANE.read_text())}
        reachable = sum(1 for p in admin if tail(p) in plane)
        print(
            f"OPERATOR_REACHABILITY=PASS admin={len(admin)} exposed={reachable} "
            f"read_only_by_design={len(EXPECTED_READ_ONLY)}"
        )
    else:
        print("OPERATOR_REACHABILITY=FAIL")
        sys.exit(1)
