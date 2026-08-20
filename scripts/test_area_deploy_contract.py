from pathlib import Path

root = Path(__file__).resolve().parents[1]
ctl = (root / "crowdrelayctl").read_text()
compose = (root / "compose.area-management.yaml").read_text()
caddy = (root / "deploy/area-management.Caddyfile").read_text()

assert "CROWDRELAY_AREA_MANAGEMENT_ENABLED" in ctl
assert "CROWDRELAY_AREA_MANAGEMENT_CONFIG_SHA256" in ctl
assert "sha256_file" in ctl
assert "sha256_stdin" in ctl
assert "verify_management_proxy" in ctl
assert "MANAGEMENT_PROXY=PASS" in ctl
assert "deploy_services+=(area-management-proxy)" in ctl
assert "compose run --rm --no-deps --entrypoint caddy area-management-proxy" in ctl
assert "area-management-proxy:" in compose
assert "${CROWDRELAY_AREA_MANAGEMENT_CONFIG_SHA256:?management config digest required}" in compose
assert "org.crowdrelay.area-management-config-sha256" in compose
assert "CROWDRELAY_CONTROL_PLANE_AREA_API_KEY" in compose
assert "CROWDRELAY_CONTROL_PLANE_API_KEY" in compose
assert "CROWDRELAY_AREA_MANAGEMENT_BIND_IP" in compose
assert "caddy@sha256:" in compose
assert "/v1/control-plane/area" in caddy
assert "/v1/control-plane/ops/summary" in caddy
assert "/v1/control-plane/ecosystem/flags" in caddy
assert "/v1/control-plane/autopilot/overview" in caddy
assert "respond 404" in caddy

print("AREA_DEPLOY_CONTRACT=PASS config-recreate=runtime-digest canonical-engine=verified")
