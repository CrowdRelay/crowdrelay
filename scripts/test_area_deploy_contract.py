from pathlib import Path
import hashlib
import re

root = Path(__file__).resolve().parents[1]

ctl = (root / "crowdrelayctl").read_text()
compose = (root / "compose.area-management.yaml").read_text()
caddy_path = root / "deploy/area-management.Caddyfile"
caddy = caddy_path.read_text()
caddy_sha256 = hashlib.sha256(caddy_path.read_bytes()).hexdigest()

assert "CROWDRELAY_AREA_MANAGEMENT_ENABLED" in ctl
assert '--file "$ROOT_DIR/compose.area-management.yaml"' in ctl
assert "deploy_services+=(area-management-proxy)" in ctl
assert "--remove-orphans" in ctl
assert 'cp "$ROOT_DIR/compose.area-management.yaml"' in ctl
assert 'cp "$ROOT_DIR/deploy/area-management.Caddyfile"' in ctl

assert "area-management-proxy:" in compose
assert "CROWDRELAY_CONTROL_PLANE_AREA_API_KEY" in compose
assert "CROWDRELAY_AREA_MANAGEMENT_BIND_IP" in compose
assert (
    "${CROWDRELAY_AREA_MANAGEMENT_BIND_IP:?AREA management bind IP missing}"
    ":18080:18080/tcp"
) in compose
assert re.search(
    r"(?<![0-9.])(?:[0-9]{1,3}\\.){3}[0-9]{1,3}:18080(?=:|[\\\"\\s])",
    compose,
) is None
assert "caddy@sha256:" in compose

match = re.search(
    r'org\.crowdrelay\.area-management-config-sha256:\s*["\']([0-9a-f]{64})["\']',
    compose,
)
assert match is not None, "management proxy must carry the tracked Caddyfile digest"
assert match.group(1) == caddy_sha256, (
    "management proxy config digest is stale: "
    f"compose={match.group(1)} caddy={caddy_sha256}"
)

assert "@area path /v1/control-plane/area /v1/control-plane/area/*" in caddy
assert "/v1/control-plane/ops/summary" in caddy
assert "/v1/control-plane/ecosystem/flags" in caddy
assert "/v1/control-plane/autopilot/overview" in caddy
assert "respond 404" in caddy

print("AREA_DEPLOY_CONTRACT=PASS config-recreate=digest-coupled")
