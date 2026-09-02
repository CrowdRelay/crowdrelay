#!/usr/bin/env python3
"""Every registered control-plane route must be reachable.

`/v1/control-plane/` requests are gated by `is_control_plane_management_path`
in `crowdrelay-api/src/lib.rs`. Registering a route in a router is not enough:
a path missing from that function is unreachable, and — because the gate runs
before routing — it answers **404**, not 401 or 403. So the failure reads as
"this endpoint does not exist" when the endpoint exists and was refused.

The consequence is worse than unreachability. `privileged` is computed from
these same predicates, so a control-plane path the gate does not recognise is
not refused — it is served **with no authentication at all**:

    let privileged = ... || is_control_plane_management_path(path);

`.../communities/{id}/intro-draft` and `.../communities/{id}/membership`
shipped that way and answered `200` with data to an unauthenticated request,
the second of them a write. Nothing compared the two lists, and `cargo check`
cannot: one side is a router, the other a `matches!` arm, and both compile
happily while disagreeing.

This compares them. Every registered `/v1/control-plane/...` literal in the
governed family must be matched by the allowlist — as an exact string, a
`one_segment_with_suffix` prefix/suffix pair, or a `starts_with` prefix.
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
    unrelated predicates, which makes the allowlist look far broader than it
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


def allowlist() -> tuple[set[str], list[tuple[str, str]], list[str]]:
    """Exact paths, (prefix, suffix) pairs, and bare prefixes."""
    gate = gate_source()
    exact = set(re.findall(r'"(/v1/control-plane/[^"]*)"(?!\s*,)', gate))
    exact |= set(re.findall(r'\|\s*"(/v1/control-plane/[^"]*)"', gate))
    exact |= set(re.findall(r'path == "(/v1/control-plane/[^"]*)"', gate))
    pairs = re.findall(
        r'one_segment_with_suffix\(\s*path,\s*"([^"]+)",\s*"([^"]+)",?\s*\)',
        gate,
    )
    prefixes = re.findall(r'path\.starts_with\("([^"]+)"\)', gate)
    return exact, pairs, prefixes


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


def is_allowed(path: str) -> bool:
    exact, pairs, prefixes = allowlist()
    if path in exact:
        return True
    if any(path.startswith(p) for p in prefixes):
        return True
    for prefix, suffix in pairs:
        if not path.startswith(prefix) or not path.endswith(suffix):
            continue
        middle = path[len(prefix) : len(path) - len(suffix)]
        # `one_segment_with_suffix` allows exactly one path segment between
        # the two, which is what `{id}` renders as.
        if middle and "/" not in middle:
            return True
    return False


class ControlPlanePathAllowlist(unittest.TestCase):
    def test_routes_are_discoverable(self) -> None:
        self.assertTrue(registered_routes(), "no control-plane routes found to check")

    def test_the_gate_is_still_where_we_look_for_it(self) -> None:
        self.assertIn("one_segment_with_suffix", gate_source())

    def test_every_registered_route_is_reachable(self) -> None:
        unreachable = sorted(p for p in registered_routes() if not is_allowed(p))
        self.assertEqual(
            unreachable,
            [],
            "these control-plane routes are registered but missing from "
            "is_control_plane_management_path, so they answer 404 as though "
            "they did not exist: " + ", ".join(unreachable),
        )


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    if result.wasSuccessful():
        print(f"CONTROL_PLANE_PATH_ALLOWLIST=PASS routes={len(registered_routes())}")
    else:
        print("CONTROL_PLANE_PATH_ALLOWLIST=FAIL")
        sys.exit(1)
