from pathlib import Path

root = Path(__file__).resolve().parents[1]

ctl = (root / "crowdrelayctl").read_text()
compose = (root / "compose.area-management.yaml").read_text()
caddy = (root / "deploy/area-management.Caddyfile").read_text()

assert "CROWDRELAY_AREA_MANAGEMENT_ENABLED" in ctl
assert '--file "$ROOT_DIR/compose.area-management.yaml"' in ctl
assert "deploy_services+=(area-management-proxy)" in ctl
assert "--remove-orphans" in ctl
assert 'cp "$ROOT_DIR/compose.area-management.yaml"' in ctl
assert 'cp "$ROOT_DIR/deploy/area-management.Caddyfile"' in ctl

assert "area-management-proxy:" in compose
assert "CROWDRELAY_CONTROL_PLANE_AREA_API_KEY" in compose
assert "CROWDRELAY_AREA_MANAGEMENT_BIND_IP" in compose
assert "100.67.186.0" not in compose
assert "0.0.0.0:18080" not in compose
assert "caddy@sha256:" in compose

assert "@area path /v1/control-plane/area /v1/control-plane/area/*" in caddy
assert "respond 404" in caddy

print("AREA_DEPLOY_CONTRACT=PASS")
