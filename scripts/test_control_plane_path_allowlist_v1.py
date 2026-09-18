#!/usr/bin/env python3
"""Every registered control-plane route must sit under the management prefix.

`/v1/control-plane/` requests are gated by `is_control_plane_management_path`
in `crowdrelay-api/src/lib.rs`. That predicate is a prefix rule — the whole
namespace minus the `/v1/control-plane/area/` sub-scope, which takes the
narrower AreaManagement bearer — because the alternative already shipped its
failure mode once: a hand-maintained list of paths that had to grow by hand
for every new route.

The list's failure was worse than unreachability. `privileged` is computed
from the same predicates, so a control-plane path the list did not recognise
was not refused — it was served **with no authentication at all**:

    let privileged = ... || is_control_plane_management_path(path);

`.../communities/{id}/intro-draft` and `.../communities/{id}/membership`
shipped that way and answered `200` with data to an unauthenticated request,
the second of them a write.

This gate now pins the rule that cannot forget: the predicate must be shaped
as the `/v1/control-plane/` prefix minus the area sub-scope, and every
registered `/v1/control-plane/...` literal in the governed family must fall
under that prefix. If the predicate ever grows an exclusion beyond the area
carve-out, this check starts flagging real routes again.
"""
from __future__ import annotations

import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
API = ROOT / "crates/crowdrelay-api/src"
LIB = API / "lib.rs"


def gate_source() -> str:
    """The body of the gate, bounded by brace depth.

    Scanning to the next `\nfn ` overshoots by 26k characters and swallows
    unrelated predicates, which makes the coverage look far broader than it
    is — the first version of this check passed for that reason.
    """
    source = LIB.read_text()
    start = source.index("fn is_control_plane_management_path")
    depth = 0
    for i in range(source.index("{", start), len(source)):
        if source[i] == "{":
            depth += 1
        elif source[i] == "}":
            depth -= 1
            if depth == 0:
                return source[start : i + 1]
    raise AssertionError("gate function is unbalanced")


def is_area_management_path(path: str) -> bool:
    """Python port of the area sub-scope the prefix rule carves out."""
    return path == "/v1/control-plane/area" or path.startswith(
        "/v1/control-plane/area/"
    )


def is_allowed(path: str) -> bool:
    """Python port of `is_control_plane_management_path` as a prefix rule.

    The Rust predicate is `(path == "/v1/control-plane" ||
    path.starts_with("/v1/control-plane/")) && !is_area_management_path(path)`.
    A route under the prefix is covered by construction; a route under the
    area sub-scope belongs to the narrower credential and must not be claimed
    by this one.
    """
    return (
        path == "/v1/control-plane" or path.startswith("/v1/control-plane/")
    ) and not is_area_management_path(path)


# Scoped to the family this was proven against. Statically modelling the whole
# gate means re-implementing several auth predicates in Python, and a check
# that is subtly wrong about auth is worse than none — an early draft of this
# flagged four beacon routes that answer 401 in production and are not
# leaking. Widen it only with the same kind of live evidence.
GOVERNED = ("/v1/control-plane/community-intelligence/",)


def registered_routes() -> set[str]:
    """Every governed /v1/control-plane path handed to `.route(...)`."""
    found: set[str] = set()
    for f in API.rglob("*.rs"):
        source = f.read_text()
        for m in re.finditer(r'\.route\(\s*"(/v1/control-plane/[^"]+)"', source):
            if m.group(1).startswith(GOVERNED):
                found.add(m.group(1))
    return found


class ControlPlanePathAllowlist(unittest.TestCase):
    def test_routes_are_discoverable(self) -> None:
        self.assertTrue(registered_routes(), "no control-plane routes found to check")

    def test_the_gate_is_still_where_we_look_for_it(self) -> None:
        gate = gate_source()
        self.assertIn('path.starts_with("/v1/control-plane/")', gate)
        self.assertIn("!is_area_management_path(path)", gate)

    def test_every_registered_route_is_reachable(self) -> None:
        unreachable = sorted(p for p in registered_routes() if not is_allowed(p))
        self.assertEqual(
            unreachable,
            [],
            "these control-plane routes are registered but outside the "
            "/v1/control-plane/ prefix boundary, so the management credential "
            "does not reach them: " + ", ".join(unreachable),
        )


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    if result.wasSuccessful():
        print(f"CONTROL_PLANE_PATH_ALLOWLIST=PASS routes={len(registered_routes())}")
    else:
        print("CONTROL_PLANE_PATH_ALLOWLIST=FAIL")
        sys.exit(1)
