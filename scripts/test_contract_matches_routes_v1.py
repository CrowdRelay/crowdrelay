#!/usr/bin/env python3
"""The contract and the router must describe the same surface.

`openapi/openapi.yaml` is the supported integration boundary — CLAUDE.md says so,
and four other repositories generate clients from it. Nothing compared it to the
routes. `validate-contract-assets.ts` validates the document's own shape: that
required paths are present, that every path carries an operation, that references
resolve. All of that can pass while the contract describes an endpoint that does
not exist, or while a public endpoint exists that the contract never mentions.

Both directions are currently clean — 320 contract paths, 453 routes, zero
mismatches — so this gate holds something true rather than fixing something
broken. It is worth holding because each direction fails in a different and
expensive way:

- **A contract path no route serves** is a promise that 404s. A generated client
  compiles, ships, and fails at the call. The consumer repositories cannot detect
  it; only this comparison can.

- **A client-facing route the contract omits** is an unsupported surface that
  consumers will find and depend on anyway, and then a refactor breaks somebody
  who was never told the endpoint was not part of the boundary.

Scope, deliberately asymmetric. The first check covers every contract path: if it
is in the contract it is a promise, whatever its prefix. The second covers only
`/v1/public`, `/v1/me`, `/v1/beacon` and `/v1/staff` — the surfaces a client
outside this repository calls. `/v1/admin`, `/v1/control-plane` and `/v1/internal`
are operator and service-to-service surfaces whose absence from the contract is a
decision, not an oversight; CLAUDE.md keeps those authority boundaries separate on
purpose, and demanding they all be published would invert that.

Path parameters are compared by position rather than by name, so renaming
`{event_id}` to `{eventId}` in one place does not read as a missing endpoint.
"""
import re
import unittest
from pathlib import Path

try:
    import yaml
except ImportError:  # pragma: no cover - the runner always has it
    yaml = None

ROOT = Path(__file__).resolve().parents[1]
API = ROOT / "crates/crowdrelay-api/src"
SPEC = ROOT / "openapi/openapi.yaml"

ROUTE_PATH = re.compile(r'\.route\(\s*"([^"]+)"')
PARAM = re.compile(r"\{[^}]+\}")

# The nine files CLAUDE.md says hold every route. Checked by
# `test_claude_md_counts_v1.py`, so a tenth cannot appear unnoticed.
ROUTE_FILES = [
    "routing.rs",
    "control_plane.rs",
    "ops_routes.rs",
    "area_admin.rs",
    "synesthesia.rs",
    "portfolio.rs",
    "audience_graph.rs",
    "community_intelligence_routes.rs",
    "routing/growth.rs",
]

# Contract paths are relative to a server base ending in `/v1`.
BASE = "/v1"

# The surfaces a client outside this repository calls.
CLIENT_PREFIXES = ("/v1/public/", "/v1/me/", "/v1/beacon/", "/v1/staff/")


def normalise(path: str) -> str:
    """Compare parameters by position, not by name."""
    return PARAM.sub("{}", path)


@unittest.skipIf(yaml is None, "PyYAML is required to parse the contract")
class ContractMatchesRoutes(unittest.TestCase):
    def setUp(self) -> None:
        self.routes = set()
        for name in ROUTE_FILES:
            self.routes |= set(ROUTE_PATH.findall((API / name).read_text()))
        document = yaml.safe_load(SPEC.read_text())
        self.spec_paths = set(document.get("paths") or {})
        self.route_shapes = {normalise(r) for r in self.routes}
        self.spec_shapes = {normalise(BASE + p) for p in self.spec_paths}

    def test_both_sides_were_actually_read(self):
        """Two empty sets match perfectly and prove nothing."""
        self.assertGreater(len(self.routes), 400, "route extraction looks broken")
        self.assertGreater(len(self.spec_paths), 300, "contract parse looks broken")

    def test_every_contract_path_has_a_route(self):
        broken = sorted(
            path
            for path in self.spec_paths
            if normalise(BASE + path) not in self.route_shapes
        )
        self.assertEqual(
            broken,
            [],
            "the contract promises these and no route serves them, so a "
            "generated client compiles, ships and 404s at the call:\n  "
            + "\n  ".join(broken),
        )

    def test_every_client_facing_route_is_in_the_contract(self):
        undocumented = sorted(
            route
            for route in self.routes
            if route.startswith(CLIENT_PREFIXES)
            and normalise(route) not in self.spec_shapes
        )
        self.assertEqual(
            undocumented,
            [],
            "these are reachable by a client outside this repository and are "
            "not in the contract, so consumers will depend on them without "
            "being told they are unsupported:\n  " + "\n  ".join(undocumented),
        )

    def test_the_contract_declares_a_v1_server_base(self):
        """The comparison above is wrong if the base ever changes."""
        document = yaml.safe_load(SPEC.read_text())
        servers = document.get("servers") or []
        self.assertTrue(servers, "the contract declares no servers")
        for server in servers:
            self.assertTrue(
                str(server.get("url", "")).rstrip("/").endswith(BASE),
                f"every server URL must end in {BASE}, or contract paths no "
                f"longer line up with routes: {server.get('url')}",
            )


if __name__ == "__main__":
    unittest.main()
