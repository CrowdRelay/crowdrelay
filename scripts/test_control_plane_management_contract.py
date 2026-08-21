#!/usr/bin/env python3
"""Keep the Control Plane runtime channel narrow and independently authenticated."""
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")

router = read("crates/crowdrelay-api/src/control_plane.rs")
lib = read("crates/crowdrelay-api/src/lib.rs")
ecosystem = read("crates/crowdrelay-infra/src/ecosystem.rs")
config = read("crates/crowdrelay-infra/src/config.rs")
compose = read("compose.area-management.yaml")
caddy = read("deploy/area-management.Caddyfile")

# Derive the route set from the router. Adding a management endpoint must not
# silently bypass either the route-local credential guard or the private tunnel.
routes = re.findall(r'\.route\(\s*"(/v1/control-plane/[^"]+)"', router)
assert routes, "no control-plane routes found in the router"
assert len(routes) == len(set(routes)), "duplicate control-plane route"

assert "MAX_CONTROL_BODY_BYTES: usize = 8 * 1024" in router
assert "DefaultBodyLimit::max(MAX_CONTROL_BODY_BYTES)" in router
assert ".route_layer(from_fn_with_state(state.clone(), require_control_plane))" in router
require_block = router.split("async fn require_control_plane", 1)[1]
assert "security::bearer_sha256_matches" in require_block
assert "state.control_plane_api_key_sha256" in require_block
assert "Problem::unauthorized" in require_block

# The global middleware still owns credential separation and request metadata,
# but route-local auth is the final fail-closed boundary for this router.
assert "is_control_plane_management_path" in lib
assert 'path.starts_with("/v1/control-plane/ops/")' in lib
assert "state.control_plane_api_key_sha256" in lib
assert "state.area_management_api_key_sha256" in lib

router_code = "\n".join(line.split("//", 1)[0] for line in router.splitlines())
assert "/v1/admin" not in router_code
assert "CROWDRELAY_CONTROL_PLANE_API_KEY" in config
assert "CROWDRELAY_CONTROL_PLANE_AREA_API_KEY" in config
assert "CROWDRELAY_CONTROL_PLANE_API_KEY" in compose

assert "@operations path" in caddy
operations_matcher = caddy.split("@operations path", 1)[1].split("handle @", 1)[0]
tunnel_paths = set(re.findall(r"/v1/control-plane/[^\\\s]+", operations_matcher))
assert tunnel_paths, "no control-plane paths found in Caddy operations matcher"

def tunnel_covers(path: str) -> bool:
    concrete_prefix = path.split("{", 1)[0] if "{" in path else path
    if concrete_prefix in tunnel_paths:
        return True
    return any(
        candidate.endswith("/*") and concrete_prefix.startswith(candidate[:-1])
        for candidate in tunnel_paths
    )

for path in routes:
    assert tunnel_covers(path), f"tunnel missing route coverage: {path}"
assert "/v1/admin" not in caddy

assert "command.expected_version" in ecosystem
assert "ecosystem_feature_flags.version = $6" in ecosystem
print(
    f"CROWDRELAY_CONTROL_PLANE=PASS routes={len(routes)} auth=route-local+separate-key+fail-closed"
    " body<=8KiB admin-alias=forbidden tunnel=derived"
)
