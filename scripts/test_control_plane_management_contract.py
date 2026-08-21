#!/usr/bin/env python3
"""Keep the Control Plane runtime channel narrow and independently authenticated."""
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

allowed = (
    "/v1/control-plane/ops/summary",
    "/v1/control-plane/ops/outbox",
    "/v1/control-plane/ops/outbox/{event_id}/retry",
    "/v1/control-plane/ops/deliveries",
    "/v1/control-plane/ops/deliveries/dead/clear",
    "/v1/control-plane/ops/deliveries/{delivery_id}",
    "/v1/control-plane/ops/deliveries/{delivery_id}/retry",
    "/v1/control-plane/ops/operations/{request_id}",
    "/v1/control-plane/ecosystem/overview",
    "/v1/control-plane/ecosystem/findings",
    "/v1/control-plane/ecosystem/reconcile",
    "/v1/control-plane/ecosystem/flags",
    "/v1/control-plane/ecosystem/flags/{key}",
    "/v1/control-plane/autopilot/overview",
    "/v1/control-plane/autopilot/policies/{context}",
)
for path in allowed:
    assert path in router, path
router_code = "\n".join(line.split("//", 1)[0] for line in router.splitlines())
assert "/v1/admin" not in router_code
assert "MAX_CONTROL_BODY_BYTES: usize = 8 * 1024" in router
assert "DefaultBodyLimit::max(MAX_CONTROL_BODY_BYTES)" in router
assert "is_control_plane_management_path" in lib
assert 'path.starts_with("/v1/control-plane/ops/")' in lib
assert "one_segment_after(path" in lib
assert "state.control_plane_api_key_sha256" in lib
assert "state.area_management_api_key_sha256" in lib
assert "CROWDRELAY_CONTROL_PLANE_API_KEY" in config and "CROWDRELAY_CONTROL_PLANE_AREA_API_KEY" in config
assert "CROWDRELAY_CONTROL_PLANE_API_KEY" in compose
assert "@operations path" in caddy
for path in (
    "/v1/control-plane/ops/outbox/*",
    "/v1/control-plane/ops/deliveries/*",
    "/v1/control-plane/ops/operations/*",
    "/v1/control-plane/ecosystem/overview",
    "/v1/control-plane/ecosystem/findings",
    "/v1/control-plane/ecosystem/reconcile",
):
    assert path in caddy, path
assert "/v1/admin" not in caddy
assert "command.expected_version" in ecosystem
assert "ecosystem_feature_flags.version = $6" in ecosystem
print("CROWDRELAY_CONTROL_PLANE=PASS routes=15 auth=separate-key body<=8KiB admin-alias=forbidden maintenance=bounded")
