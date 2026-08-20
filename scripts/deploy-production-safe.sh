#!/usr/bin/env bash
set -Eeuo pipefail
umask 077

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
TARGET="${1:-}"
ORACLE="${CROWDRELAY_DEPLOY_HOST:-virya-oracle}"
ORACLE_REPO="${CROWDRELAY_DEPLOY_REMOTE_REPO:-/opt/crowdrelay}"
CONTROL_PLANE_HOST="${CROWDRELAY_CONTROL_PLANE_HOST:-virya-home}"
CONTROL_PLANE_DIR="${CROWDRELAY_CONTROL_PLANE_DIR:-/srv/crowdrelay-control-plane}"
CANONICAL="$ROOT_DIR/scripts/deploy-production-exact.sh"

fail() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

require() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

for command in git ssh python3; do require "$command"; done
cd "$ROOT_DIR"
[[ -x "$CANONICAL" ]] || fail "canonical exact deploy is missing or not executable: $CANONICAL"
[[ -z "$(git status --porcelain --untracked-files=normal)" ]] || fail 'local worktree must be clean'
branch="$(git symbolic-ref --quiet --short HEAD 2>/dev/null || true)"
[[ "$branch" == "main" ]] || fail "production deploy must run from main, got=${branch:-detached}"

HEAD_SHA="$(git rev-parse HEAD)"
if [[ -z "$TARGET" ]]; then
  TARGET="$HEAD_SHA"
fi
[[ "$TARGET" =~ ^[0-9a-f]{40}$ ]] || fail 'target must be a full lowercase 40-character SHA'
[[ "$TARGET" == "$HEAD_SHA" ]] || fail "target must equal local HEAD: target=$TARGET head=$HEAD_SHA"
REMOTE_MAIN="$(git ls-remote origin refs/heads/main | awk '{print $1}')"
[[ "$REMOTE_MAIN" == "$TARGET" ]] || fail "origin/main mismatch: remote=$REMOTE_MAIN local=$TARGET"

printf '==> 0/3 — Cross-system source contracts\n'
python3 scripts/test_area_deploy_contract.py
python3 scripts/test_control_plane_management_contract.py
python3 scripts/test_boring_production_deploy_contract.py
printf 'SOURCE_CONTRACTS=PASS\n'

printf '\n==> 1/3 — Canonical exact-SHA CrowdRelay deploy\n'
"$CANONICAL" "$TARGET"

printf '\n==> 2/3 — Force-refresh and verify Oracle management proxy\n'
ssh -T "$ORACLE" bash -s -- "$ORACLE_REPO" "$TARGET" <<'ORACLE_GATE'
set -Eeuo pipefail
repo="$1"
target="$2"
cd "$repo"

fail() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

[[ "$(git rev-parse HEAD)" == "$target" ]] || fail 'Oracle source HEAD drifted after canonical deploy'
[[ -z "$(git status --porcelain --untracked-files=normal)" ]] || fail 'Oracle worktree is dirty after canonical deploy'
[[ -f .crowdrelay.local.sh && ! -L .crowdrelay.local.sh ]] || fail 'server-local CrowdRelay config is missing or unsafe'
# shellcheck source=/dev/null
source .crowdrelay.local.sh
[[ "${CROWDRELAY_AREA_MANAGEMENT_ENABLED:-false}" == "true" ]] || fail 'AREA management must stay enabled in production'

./crowdrelayctl doctor

env_file="${CROWDRELAY_ENV_FILE:-deploy/.env.production}"
compose_file="${CROWDRELAY_COMPOSE_FILE:-compose.production.yaml}"
[[ -f "$env_file" && ! -L "$env_file" ]] || fail "production env is missing or unsafe: $env_file"
[[ -f "$compose_file" && ! -L "$compose_file" ]] || fail "production compose is missing or unsafe: $compose_file"
[[ -f compose.area-management.yaml && ! -L compose.area-management.yaml ]] || fail 'AREA compose overlay is missing or unsafe'
[[ -f deploy/area-management.Caddyfile && ! -L deploy/area-management.Caddyfile ]] || fail 'AREA Caddyfile is missing or unsafe'

compose() {
  docker compose \
    --env-file "$env_file" \
    -f "$compose_file" \
    -f compose.area-management.yaml \
    "$@"
}

compose config --quiet
compose up -d --no-deps --force-recreate area-management-proxy

for _ in $(seq 1 30); do
  status="$(docker inspect crowdrelay-area-management-proxy-1 --format '{{.State.Status}}' 2>/dev/null || true)"
  [[ "$status" == "running" ]] && break
  sleep 1
done
[[ "$status" == "running" ]] || fail "management proxy failed to start: $status"

runtime_sha="$(docker exec crowdrelay-area-management-proxy-1 cat /etc/caddy/Caddyfile | sha256sum | awk '{print $1}')"
source_sha="$(sha256sum deploy/area-management.Caddyfile | awk '{print $1}')"
[[ "$runtime_sha" == "$source_sha" ]] || fail 'live management Caddyfile differs from source'
docker exec crowdrelay-area-management-proxy-1 caddy validate --config /etc/caddy/Caddyfile >/dev/null
docker exec crowdrelay-area-management-proxy-1 cat /etc/caddy/Caddyfile | grep -Fq '/v1/control-plane/ops/summary' || fail 'live management proxy is missing operations route'

endpoint="$(docker port crowdrelay-area-management-proxy-1 18080/tcp | head -n1)"
[[ -n "$endpoint" ]] || fail 'management proxy has no published private endpoint'
status_code="$(curl -sS -o /dev/null -w '%{http_code}' --connect-timeout 3 --max-time 10 "http://${endpoint}/v1/control-plane/ops/summary")"
[[ "$status_code" == "401" ]] || fail "management proxy routing gate failed: expected=401 got=$status_code"

for service in api worker; do
  container="crowdrelay-${service}-1"
  image_id="$(docker inspect "$container" --format '{{.Image}}')"
  revision="$(docker image inspect "$image_id" --format '{{index .Config.Labels "org.opencontainers.image.revision"}}')"
  [[ "$revision" == "$target" ]] || fail "runtime revision mismatch for $service: $revision"
done

printf 'ORACLE_MANAGEMENT_PROXY=PASS sha=%s routing=401 config=current\n' "$target"
ORACLE_GATE

printf '\n==> 3/3 — Control Plane cross-system E2E\n'
ssh -T "$CONTROL_PLANE_HOST" sudo bash -s -- "$CONTROL_PLANE_DIR" <<'CONTROL_PLANE_GATE'
set -Eeuo pipefail
root="$1"
cd "$root"

fail() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

app="crowdrelay-control-plane-app-1"
tunnel="crowdrelay-control-plane-virya-area-tunnel-1"
[[ "$(docker inspect "$app" --format '{{.State.Status}}')" == "running" ]] || fail 'Control Plane app is not running'
[[ "$(docker inspect "$tunnel" --format '{{.State.Status}}')" == "running" ]] || fail 'Control Plane tunnel is not running'
app_id="$(docker inspect "$app" --format '{{.Id}}')"
network_mode="$(docker inspect "$tunnel" --format '{{.HostConfig.NetworkMode}}')"
[[ "$network_mode" == "container:${app_id}" ]] || fail "Control Plane tunnel namespace drift: $network_mode"
docker exec "$tunnel" cat /etc/caddy/Caddyfile | grep -Fq '/v1/control-plane/ops/summary' || fail 'Control Plane tunnel lost operations route'

published="$(docker port "$app" 8090/tcp | head -n1)"
[[ -n "$published" ]] || fail 'Control Plane app has no published endpoint'
admin="$(docker inspect "$app" --format '{{range .Config.Env}}{{println .}}{{end}}' | sed -n 's/^CONTROL_PLANE_ADMIN_TOKEN=//p')"
[[ -n "$admin" ]] || fail 'Control Plane admin token missing from runtime'
summary="$(curl -fsS --connect-timeout 3 --max-time 10 -H "Authorization: Bearer $admin" "http://${published}/api/v1/tenants/virya/operations/summary")"
printf '%s' "$summary" | python3 -c '
import json
import sys
value = json.load(sys.stdin)
if not isinstance(value, dict):
    raise SystemExit("operations summary is not an object")
if not isinstance(value.get("schema_version"), int):
    raise SystemExit("schema_version missing")
http = value.get("http")
if not isinstance(http, dict) or not isinstance(http.get("p95_ms"), int):
    raise SystemExit("http.p95_ms missing")
print("CONTROL_PLANE_CROSS_GATE=PASS schema={} p95_ms={}".format(value["schema_version"], http["p95_ms"]))
'
CONTROL_PLANE_GATE

printf '\nCROWDRELAY_SAFE_DEPLOY=PASS sha=%s oracle_proxy=current control_plane_e2e=pass\n' "$TARGET"
