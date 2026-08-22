#!/usr/bin/env bash
set -Eeuo pipefail
umask 077

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
TARGET="${1:-}"
ORACLE="${CROWDRELAY_DEPLOY_HOST:-virya-crowdrelay}"
ORACLE_REPO="${CROWDRELAY_DEPLOY_REMOTE_REPO:-/opt/crowdrelay}"
CONTROL_PLANE_HOST="${CROWDRELAY_CONTROL_PLANE_HOST:-virya-crowdrelay}"
CONTROL_PLANE_DIR="${CROWDRELAY_CONTROL_PLANE_DIR:-/srv/crowdrelay-control-plane}"
CANONICAL="$ROOT_DIR/scripts/deploy-production-exact.sh"

fail() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

require() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

for command in git ssh python3 bash; do require "$command"; done
cd "$ROOT_DIR"
[[ -f "$CANONICAL" && ! -L "$CANONICAL" ]] || fail "canonical exact deploy is missing or unsafe: $CANONICAL"
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
bash "$CANONICAL" "$TARGET"

printf '\n==> 2/3 — Verify Oracle management proxy\n'
ssh -T "$ORACLE" bash -s -- "$ORACLE_REPO" "$TARGET" <<'ORACLE_GATE'
# Remote body runs as one brace group with stdin detached. bash reads this
# script from ssh stdin; any command that attaches stdin (docker compose
# run/exec) would otherwise swallow the remainder and silently skip it.
{
set -Eeuo pipefail
repo="$1"
target="$2"
cd "$repo"

fail() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

for command in docker curl sha256sum grep; do
  command -v "$command" >/dev/null 2>&1 || fail "missing Oracle command: $command"
done
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

for route in \
  '/v1/control-plane/area' \
  '/v1/control-plane/ops/summary' \
  '/v1/control-plane/ecosystem/flags' \
  '/v1/control-plane/autopilot/overview'; do
  grep -Fq "$route" deploy/area-management.Caddyfile || fail "source management Caddyfile is missing route: $route"
done

compose() {
  docker compose \
    --env-file "$env_file" \
    -f "$compose_file" \
    -f compose.area-management.yaml \
    "$@"
}

# The area overlay pins the proxy to the exact Caddyfile digest so a config
# change forces recreation. That digest is derived, not stored in the env
# file, so it must be exported before any compose call can interpolate the
# overlay -- otherwise `compose config` fails on a missing required variable.
source_sha="$(sha256sum deploy/area-management.Caddyfile | cut -d " " -f1)"
export CROWDRELAY_AREA_MANAGEMENT_CONFIG_SHA256="$source_sha"

compose config --quiet
proxy_status="$(docker inspect crowdrelay-area-management-proxy-1 --format "{{.State.Status}}" 2>/dev/null || true)"
[[ "$proxy_status" == "running" ]] || fail "management proxy is not running after canonical deploy: $proxy_status"
runtime_sha="$(docker exec crowdrelay-area-management-proxy-1 cat /etc/caddy/Caddyfile | sha256sum | cut -d " " -f1 || true)"
[[ "$runtime_sha" == "$source_sha" ]] || fail "canonical deploy left stale management Caddyfile: runtime=$runtime_sha source=$source_sha"
printf "ORACLE_MANAGEMENT_RECONCILE=VERIFY_ONLY config=current\n"

for _ in $(seq 1 30); do
  status="$(docker inspect crowdrelay-area-management-proxy-1 --format '{{.State.Status}}' 2>/dev/null || true)"
  [[ "$status" == "running" ]] && break
  sleep 1
done
[[ "$status" == "running" ]] || fail "management proxy failed to start: $status"

runtime_caddy="$(docker exec crowdrelay-area-management-proxy-1 cat /etc/caddy/Caddyfile)" \
  || fail 'cannot read live management Caddyfile'
runtime_sha="$(printf '%s\n' "$runtime_caddy" | sha256sum | awk '{print $1}')"
[[ "$runtime_sha" == "$source_sha" ]] || fail 'live management Caddyfile differs from source'
docker exec crowdrelay-area-management-proxy-1 caddy validate --config /etc/caddy/Caddyfile >/dev/null
for route in \
  '/v1/control-plane/area' \
  '/v1/control-plane/ops/summary' \
  '/v1/control-plane/ecosystem/flags' \
  '/v1/control-plane/autopilot/overview'; do
  grep -Fq "$route" <<<"$runtime_caddy" || fail "live management proxy is missing route: $route"
done

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
} </dev/null
ORACLE_GATE

printf '\n==> 3/3 — Control Plane cross-system E2E\n'
ssh -T "$CONTROL_PLANE_HOST" sudo bash -s -- "$CONTROL_PLANE_DIR" <<'CONTROL_PLANE_GATE'
# Remote body runs as one brace group with stdin detached. bash reads this
# script from ssh stdin; any command that attaches stdin (docker compose
# run/exec) would otherwise swallow the remainder and silently skip it.
{
set -Eeuo pipefail
root="$1"
cd "$root"

fail() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

for command in docker curl python3 grep; do
  command -v "$command" >/dev/null 2>&1 || fail "missing Control Plane gate command: $command"
done

app="crowdrelay-control-plane-app-1"
tunnel="crowdrelay-control-plane-virya-area-tunnel-1"
[[ "$(docker inspect "$app" --format '{{.State.Status}}')" == "running" ]] || fail 'Control Plane app is not running'
[[ "$(docker inspect "$tunnel" --format '{{.State.Status}}')" == "running" ]] || fail 'Control Plane tunnel is not running'
app_id="$(docker inspect "$app" --format '{{.Id}}')"
network_mode="$(docker inspect "$tunnel" --format '{{.HostConfig.NetworkMode}}')"
[[ "$network_mode" == "container:${app_id}" ]] || fail "Control Plane tunnel namespace drift: $network_mode"
tunnel_caddy="$(docker exec "$tunnel" cat /etc/caddy/Caddyfile)" || fail 'cannot read Control Plane tunnel Caddyfile'
for route in \
  '/v1/control-plane/area' \
  '/v1/control-plane/ops/summary' \
  '/v1/control-plane/ecosystem/flags' \
  '/v1/control-plane/autopilot/overview'; do
  grep -Fq "$route" <<<"$tunnel_caddy" || fail "Control Plane tunnel lost route: $route"
done
unset tunnel_caddy

runtime_env="$(docker inspect "$app" --format '{{range .Config.Env}}{{println .}}{{end}}')"
area_master="$(printf '%s\n' "$runtime_env" | sed -n 's/^CONTROL_PLANE_AREA_MANAGEMENT_MASTER_KEY=//p')"
management_master="$(printf '%s\n' "$runtime_env" | sed -n 's/^CONTROL_PLANE_MANAGEMENT_MASTER_KEY=//p')"
management_url="$(printf '%s\n' "$runtime_env" | sed -n 's/^CONTROL_PLANE_VIRYA_MANAGEMENT_URL=//p')"
[[ -n "$area_master" ]] || fail 'Control Plane AREA management master is missing from runtime'
[[ -n "$management_master" ]] || fail 'Control Plane operations management master is missing from runtime'
[[ "$management_url" == "http://127.0.0.1:18080" ]] || fail "Control Plane management URL drifted: $management_url"
unset runtime_env area_master management_master management_url

published="$(docker port "$app" 8090/tcp | head -n1)"
[[ -n "$published" ]] || fail 'Control Plane app has no published endpoint'
admin="$(docker inspect "$app" --format '{{range .Config.Env}}{{println .}}{{end}}' | sed -n 's/^CONTROL_PLANE_ADMIN_TOKEN=//p')"
[[ -n "$admin" ]] || fail 'Control Plane admin token missing from runtime'
summary=""
for attempt in $(seq 1 30); do
  body_file="$(mktemp)"
  code="000"
  if code="$(curl -sS -o "$body_file" -w '%{http_code}' --connect-timeout 3 --max-time 10 \
    -H "Authorization: Bearer $admin" \
    "http://${published}/api/v1/tenants/virya/operations/summary")"; then
    if [[ "$code" == "200" ]]; then
      summary="$(cat "$body_file")"
      rm -f "$body_file"
      printf 'CONTROL_PLANE_CROSS_GATE_READINESS=PASS attempt=%s status=200\n' "$attempt"
      break
    fi
    if [[ ! "$code" =~ ^(502|503|504)$ ]]; then
      detail="$(cat "$body_file" 2>/dev/null || true)"
      rm -f "$body_file"
      fail "Control Plane cross-system gate failed status=$code detail=$detail"
    fi
  fi
  rm -f "$body_file"
  if [[ "$attempt" == "30" ]]; then
    fail "Control Plane cross-system gate did not converge after transient management-proxy failures"
  fi
  printf 'CONTROL_PLANE_CROSS_GATE_RETRY attempt=%s status=%s\n' "$attempt" "$code" >&2
  sleep 1
done
unset admin
[[ -n "$summary" ]] || fail 'Control Plane cross-system gate returned no summary after bounded retry'
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
} </dev/null
CONTROL_PLANE_GATE

printf '\nCROWDRELAY_SAFE_DEPLOY=PASS sha=%s oracle_proxy=current control_plane_e2e=pass\n' "$TARGET"
