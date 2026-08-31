#!/usr/bin/env bash
set -Eeuo pipefail
umask 077

# Blue-green CrowdRelay deploy with zero-downtime Caddy cutover.
#
# This script runs ON the production host (virya-crowdrelay) via SSH.
#
# Alternating blue-green: detects which color is currently active and
# deploys to the other color. If blue (api/worker) is running, starts
# api-green/worker-green and switches Caddy to green. If green
# (api-green/worker-green) is running, starts api/worker (blue) and
# switches Caddy to blue.
#
# On any failure it reverts the Caddy upstream and stops the new containers,
# leaving production on the previous release with no user-visible downtime.
#
# Usage (called by deploy-ecosystem.sh or directly):
#   bash scripts/deploy-bluegreen.sh <target-sha> [repo-dir]
#
# Environment:
#   CROWDRELAY_DOCKER_NETWORK  — shared Docker network (from .crowdrelay.local.sh)
#   CROWDRELAY_ENV_FILE        — production env file
#   CROWDRELAY_COMPOSE_FILE    — production compose file
#   CROWDRELAY_PUBLIC_BASE_URL — public URL for health verification
#   CROWDRELAY_DEPLOY_WAIT_TIMEOUT_SECONDS — health-check timeout

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd -P 2>/dev/null || echo /opt/crowdrelay)"
TARGET="${1:-}"
REPO_DIR="${2:-$ROOT_DIR}"
EDGE_CADDYFILE="/opt/crowdrelay/ops/edge/Caddyfile"
EDGE_CONTAINER="virya-edge-caddy"
HEALTH_TIMEOUT="${CROWDRELAY_DEPLOY_WAIT_TIMEOUT_SECONDS:-180}"
GREEN_API="crowdrelay-api-green-1"
GREEN_WORKER="crowdrelay-worker-green-1"
BLUE_API="crowdrelay-api-1"
BLUE_WORKER="crowdrelay-worker-1"
GREEN_ALIAS="crowdrelay-api-green"
BLUE_ALIAS="crowdrelay-api"
ACTIVE_ALIAS="crowdrelay-api-active"
CADDY_BACKUP=""
NEW_STARTED=false
ALIAS_MOVED=false

fail() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

rollback() {
  local status="${1:-1}"
  trap - ERR INT TERM HUP

  if [[ "$ALIAS_MOVED" == true ]]; then
    printf 'ROLLBACK=START reason=alias-moved reverting active alias to %s\n' "${CURRENT_API:-}" >&2
    # Move the active alias back to the old container
    docker network disconnect "$CROWDRELAY_DOCKER_NETWORK" "$NEW_API" >/dev/null 2>&1 || true
    docker network connect --alias "$ACTIVE_ALIAS" "$CROWDRELAY_DOCKER_NETWORK" "$CURRENT_API" >/dev/null 2>&1 || true
    # Restore color-specific alias on old container
    if [[ "$DEPLOY_COLOR" == "green" ]]; then
      docker network connect --alias "$BLUE_ALIAS" "$CROWDRELAY_DOCKER_NETWORK" "$CURRENT_API" >/dev/null 2>&1 || true
    else
      docker network connect --alias "$GREEN_ALIAS" "$CROWDRELAY_DOCKER_NETWORK" "$CURRENT_API" >/dev/null 2>&1 || true
    fi
    printf 'ROLLBACK=ALIAS_REVERTED active=%s\n' "${CURRENT_API:-}" >&2
  fi

  if [[ "$NEW_STARTED" == true ]]; then
    printf 'ROLLBACK=STOPPING_NEW\n' >&2
    cd "$REPO_DIR"
    source .crowdrelay.local.sh 2>/dev/null || true
    local env_file compose_file
    env_file="$(absolute_path "${CROWDRELAY_ENV_FILE:-deploy/.env.production}")"
    compose_file="$(absolute_path "${CROWDRELAY_COMPOSE_FILE:-compose.production.yaml}")"
    if [[ "$DEPLOY_COLOR" == "green" ]]; then
      docker compose --env-file "$env_file" -f "$compose_file" -f compose.bluegreen.yaml \
        stop api-green worker-green >/dev/null 2>&1 || true
      docker compose --env-file "$env_file" -f "$compose_file" -f compose.bluegreen.yaml \
        rm -f api-green worker-green >/dev/null 2>&1 || true
    else
      docker compose --env-file "$env_file" -f "$compose_file" \
        stop api worker >/dev/null 2>&1 || true
      docker compose --env-file "$env_file" -f "$compose_file" \
        rm -f api worker >/dev/null 2>&1 || true
    fi
    printf 'ROLLBACK=NEW_STOPPED\n' >&2
  fi

  printf 'ROLLBACK=COMPLETE status=%d\n' "$status" >&2
  exit "$status"
}

absolute_path() {
  local path="$1"
  if [[ "$path" = /* ]]; then printf '%s\n' "$path"; else printf '%s/%s\n' "$REPO_DIR" "$path"; fi
}

# --- Pre-flight -------------------------------------------------------------

[[ "$TARGET" =~ ^[0-9a-f]{40}$ ]] || fail "usage: deploy-bluegreen.sh <full-40-char-sha>"
for command in docker curl python3; do command -v "$command" >/dev/null 2>&1 || fail "missing command: $command"; done

cd "$REPO_DIR"
[[ -f .crowdrelay.local.sh && ! -L .crowdrelay.local.sh ]] || fail "missing .crowdrelay.local.sh"
# shellcheck source=/dev/null
source .crowdrelay.local.sh

[[ -f "$EDGE_CADDYFILE" && ! -L "$EDGE_CADDYFILE" ]] || fail "missing edge Caddyfile: $EDGE_CADDYFILE"
docker inspect "$EDGE_CONTAINER" --format '{{.State.Status}}' 2>/dev/null | grep -q running || fail "edge Caddy is not running"

# Pre-deploy: verify the Caddyfile uses the stable active alias with
# dynamic a, not a color-specific name. If it doesn't, fix it before
# deploying. With dynamic a, Caddy re-resolves Docker DNS every 5s,
# so no restart is needed during cutover.
if ! grep -Fq "dynamic a ${ACTIVE_ALIAS}" "$EDGE_CADDYFILE"; then
  printf 'RECONCILE=FIX Caddyfile does not use dynamic a %s, repairing\n' "$ACTIVE_ALIAS"
  # Replace any static upstream references with dynamic a
  sed "s|reverse_proxy ${BLUE_ALIAS}:8080|reverse_proxy { dynamic a ${ACTIVE_ALIAS} { port 8080; refresh 5s } }|g; s|reverse_proxy ${GREEN_ALIAS}:8080|reverse_proxy { dynamic a ${ACTIVE_ALIAS} { port 8080; refresh 5s } }|g; s|reverse_proxy ${ACTIVE_ALIAS}:8080|reverse_proxy { dynamic a ${ACTIVE_ALIAS} { port 8080; refresh 5s } }|g" "$EDGE_CADDYFILE" > /tmp/caddy-reconcile.tmp
  cat /tmp/caddy-reconcile.tmp > "$EDGE_CADDYFILE"
  rm -f /tmp/caddy-reconcile.tmp
  # Reload Caddy with the updated host-side Caddyfile. The bind mount
  # means the container already sees the file — we just need to reload.
  # No restart needed — reload is zero-downtime.
  docker exec "$EDGE_CONTAINER" caddy reload --config /etc/caddy/Caddyfile --adapter caddyfile >/dev/null 2>&1 || \
    fail "caddy reload failed after reconciliation — investigate manually"
  sleep 2
  grep -Fq "dynamic a ${ACTIVE_ALIAS}" "$EDGE_CADDYFILE" || fail "Caddyfile reconciliation failed — cannot find dynamic a ${ACTIVE_ALIAS}"
  printf 'RECONCILE=PASS Caddyfile now uses dynamic a %s\n' "$ACTIVE_ALIAS"
fi

# Pre-deploy: reconcile edge Caddy bind mount.
# The ecosystem deploy syncs source code (git merge) which may replace the
# Caddyfile with a new inode. The Docker bind mount still points at the old
# inode, so the container serves stale config. Since the Caddyfile is
# bind-mounted, we can't docker cp over it — instead we write to the host
# path (which the bind mount sees) and reload. If the inode changed, we
# copy the content to force the bind mount to see the new content.
if ! cmp -s <(docker exec "$EDGE_CONTAINER" cat /etc/caddy/Caddyfile 2>/dev/null) "$EDGE_CADDYFILE"; then
  printf 'EDGE_RECONCILE=STALE rewriting Caddyfile on host and reloading\n' >&2
  # Copy content to a temp file, then overwrite the bind-mounted file
  cp "$EDGE_CADDYFILE" /tmp/caddy-edge-sync.tmp
  cat /tmp/caddy-edge-sync.tmp > "$EDGE_CADDYFILE"
  rm -f /tmp/caddy-edge-sync.tmp
  docker exec "$EDGE_CONTAINER" caddy reload --config /etc/caddy/Caddyfile --adapter caddyfile >/dev/null 2>&1 || \
    fail "caddy reload failed after stale bind mount fix — investigate manually"
  sleep 2
  cmp -s <(docker exec "$EDGE_CONTAINER" cat /etc/caddy/Caddyfile 2>/dev/null) "$EDGE_CADDYFILE" || \
    fail "edge Caddyfile still stale after rewrite+reload — manual intervention required"
  printf 'EDGE_RECONCILE=PASS\n'
fi

env_file="$(absolute_path "${CROWDRELAY_ENV_FILE:-deploy/.env.production}")"
compose_file="$(absolute_path "${CROWDRELAY_COMPOSE_FILE:-compose.production.yaml}")"
export CROWDRELAY_ENV_FILE="$env_file"
export CROWDRELAY_DOCKER_NETWORK
export CROWDRELAY_GREEN_TAG="sha-${TARGET}"

# Verify the new images are available
for component in api worker; do
  image="ghcr.io/crowdrelay/crowdrelay-${component}:${CROWDRELAY_GREEN_TAG}"
  revision="$(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$image" 2>/dev/null || true)"
  [[ "$revision" == "$TARGET" ]] || fail "image not available or revision mismatch: $image (got=${revision})"
done
printf 'NEW_IMAGES=PASS sha=%s\n' "$TARGET"

# Detect which color is currently active and determine deploy direction.
blue_health="$(docker inspect "$BLUE_API" --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' 2>/dev/null || true)"
green_health="$(docker inspect "$GREEN_API" --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' 2>/dev/null || true)"

if [[ "$blue_health" == "healthy" || "$blue_health" == "running" ]]; then
  DEPLOY_COLOR="green"
  CURRENT_API="$BLUE_API"
  CURRENT_WORKER="$BLUE_WORKER"
  CURRENT_ALIAS="$BLUE_ALIAS"
  NEW_API="$GREEN_API"
  NEW_WORKER="$GREEN_WORKER"
  NEW_ALIAS="$GREEN_ALIAS"
  printf 'BASELINE=BLUE health=%s → deploying green\n' "$blue_health"
elif [[ "$green_health" == "healthy" || "$green_health" == "running" ]]; then
  DEPLOY_COLOR="blue"
  CURRENT_API="$GREEN_API"
  CURRENT_WORKER="$GREEN_WORKER"
  CURRENT_ALIAS="$GREEN_ALIAS"
  NEW_API="$BLUE_API"
  NEW_WORKER="$BLUE_WORKER"
  NEW_ALIAS="$BLUE_ALIAS"
  printf 'BASELINE=GREEN health=%s → deploying blue\n' "$green_health"
else
  fail "no running API found: blue=$blue_health green=$green_health — bootstrap with deploy-home.sh first"
fi

# Snapshot the current Caddyfile for rollback
CADDY_BACKUP="$(mktemp -t caddyfile-blue.XXXXXX)"
cp "$EDGE_CADDYFILE" "$CADDY_BACKUP"
printf 'CADDY_BACKUP=PASS file=%s\n' "$CADDY_BACKUP"

trap 'rollback $?' ERR INT TERM HUP

# --- 1. Start new containers ------------------------------------------------

printf '\n==> 1/6 — Start %s api+worker\n' "$DEPLOY_COLOR"
NEW_STARTED=true

if [[ "$DEPLOY_COLOR" == "green" ]]; then
  docker compose --env-file "$env_file" -f "$compose_file" -f compose.bluegreen.yaml \
    up -d --no-deps --wait --wait-timeout "$HEALTH_TIMEOUT" api-green worker-green
else
  # Deploy blue: override the image tag without modifying .crowdrelay.local.sh
  export CROWDRELAY_IMAGE_TAG="sha-${TARGET}"
  docker compose --env-file "$env_file" -f "$compose_file" \
    up -d --no-deps --wait --wait-timeout "$HEALTH_TIMEOUT" api worker
fi

printf 'NEW_CONTAINERS=STARTED color=%s\n' "$DEPLOY_COLOR"

# --- 2. Health-check new API directly ---------------------------------------

printf '\n==> 2/6 — Health-check %s API\n' "$DEPLOY_COLOR"
new_health=""
for attempt in $(seq 1 30); do
  new_health="$(docker inspect "$NEW_API" --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' 2>/dev/null || true)"
  if [[ "$new_health" == "healthy" ]]; then
    break
  fi
  sleep 2
done
[[ "$new_health" == "healthy" ]] || fail "$DEPLOY_COLOR API did not become healthy: $new_health"

# Direct health check via network alias
docker run --rm --network "$CROWDRELAY_DOCKER_NETWORK" curlimages/curl:8.12.0 \
  --fail --silent --show-error --connect-timeout 3 --max-time 10 \
  "http://${NEW_ALIAS}:8080/v1/health/ready" >/dev/null

# Verify new API serves the correct SHA in /v1/meta
new_meta="$(docker run --rm --network "$CROWDRELAY_DOCKER_NETWORK" curlimages/curl:8.12.0 \
  --fail --silent --show-error --connect-timeout 3 --max-time 10 \
  "http://${NEW_ALIAS}:8080/v1/meta")"
printf '%s' "$new_meta" | python3 -c "
import json, sys
expected = sys.argv[1]
data = json.load(sys.stdin)
actual = data.get('gitSha', '')
if actual != expected:
    raise SystemExit(f'meta mismatch: got={actual} expected={expected}')
" "$TARGET"

printf 'NEW_HEALTH=PASS meta_sha=%s\n' "$TARGET"

# --- 3. Run migrations via setup --------------------------------------------

printf '\n==> 3/6 — Run migrations (setup)\n'
CROWDRELAY_IMAGE_TAG="$CROWDRELAY_GREEN_TAG" \
docker compose --env-file "$env_file" -f "$compose_file" \
  run --rm -T setup </dev/null
printf 'MIGRATIONS=PASS\n'

# --- 4. Move active alias to new API -----------------------------------------

printf '\n==> 4/6 — Move %s alias to %s API\n' "$ACTIVE_ALIAS" "$DEPLOY_COLOR"
# The Caddyfile uses `dynamic a crowdrelay-api-active` which re-resolves
# Docker DNS every 5s. The new container already has the active alias
# from its compose config. We just need to remove the active alias from
# the old container. Caddy will pick up the change on the next refresh.
#
# Step 4a: Remove the active alias from the old container.
# The new container already has ACTIVE_ALIAS from compose.bluegreen.yaml
# (green) or compose.production.yaml (blue), so there is no gap — both
# containers have the alias briefly, and Caddy load-balances between them.
docker network disconnect "$CROWDRELAY_DOCKER_NETWORK" "$CURRENT_API" 2>/dev/null || true
# Reconnect the old container without the active alias but keep its
# color-specific alias so it's still reachable for drain/stop.
if [[ "$DEPLOY_COLOR" == "green" ]]; then
  docker network connect --alias "$BLUE_ALIAS" "$CROWDRELAY_DOCKER_NETWORK" "$CURRENT_API" 2>/dev/null || true
else
  docker network connect --alias "$GREEN_ALIAS" "$CROWDRELAY_DOCKER_NETWORK" "$CURRENT_API" 2>/dev/null || true
fi

# Step 4b: Also add the BLUE_ALIAS (crowdrelay-api) to the new container.
# This ensures internal consumers that still reference crowdrelay-api:8080
# (like the rekor-anchor, whose image has a URL allowlist) can reach the
# new active container. Once the rekor-anchor image is rebuilt with the
# updated allowlist, this can be removed.
docker network disconnect "$CROWDRELAY_DOCKER_NETWORK" "$NEW_API" 2>/dev/null || true
if [[ "$DEPLOY_COLOR" == "green" ]]; then
  docker network connect --alias "$GREEN_ALIAS" --alias "$ACTIVE_ALIAS" --alias "$BLUE_ALIAS" "$CROWDRELAY_DOCKER_NETWORK" "$NEW_API" 2>/dev/null || true
else
  docker network connect --alias "$BLUE_ALIAS" --alias "$ACTIVE_ALIAS" --alias "$GREEN_ALIAS" "$CROWDRELAY_DOCKER_NETWORK" "$NEW_API" 2>/dev/null || true
fi

# Step 4c: Wait for Caddy's dynamic a to re-resolve DNS (refresh is 5s,
# wait two cycles to be safe). No restart or reload needed — Caddy
# automatically picks up the new container's IP.
sleep 10

# Verify the active alias resolves and serves the correct SHA.
docker run --rm --network "$CROWDRELAY_DOCKER_NETWORK" curlimages/curl:8.12.0 \
  --fail --silent --show-error --connect-timeout 3 --max-time 10 \
  "http://${ACTIVE_ALIAS}:8080/v1/health/ready" >/dev/null

active_meta="$(docker run --rm --network "$CROWDRELAY_DOCKER_NETWORK" curlimages/curl:8.12.0 \
  --fail --silent --show-error --connect-timeout 3 --max-time 10 \
  "http://${ACTIVE_ALIAS}:8080/v1/meta")"
printf '%s' "$active_meta" | python3 -c "
import json, sys
expected = sys.argv[1]
data = json.load(sys.stdin)
actual = data.get('gitSha', '')
if actual != expected:
    raise SystemExit(f'active alias meta mismatch: got={actual} expected={expected}')
" "$TARGET"

ALIAS_MOVED=true
printf 'ALIAS_MOVE=PASS active=%s container=%s\n' "$ACTIVE_ALIAS" "$NEW_API"
printf 'CADDY_DNS=PASS dynamic-a-refresh=no-restart\n'

# Also update the area management proxy Caddyfile if it references a
# color-specific alias. The area proxy should use the stable alias too.
AREA_CADDYFILE="/opt/crowdrelay/deploy/area-management.Caddyfile"
AREA_PROXY_CONTAINER="crowdrelay-area-management-proxy-1"
if [[ -f "$AREA_CADDYFILE" ]] && ! grep -Fq "dynamic a ${ACTIVE_ALIAS}" "$AREA_CADDYFILE"; then
  area_tmp="$(mktemp)"
  sed "s|http://${BLUE_ALIAS}:8080|http://${ACTIVE_ALIAS}:8080|g; s|http://${GREEN_ALIAS}:8080|http://${ACTIVE_ALIAS}:8080|g; s|http://api:8080|http://${ACTIVE_ALIAS}:8080|g" "$AREA_CADDYFILE" > "$area_tmp"
  cat "$area_tmp" > "$AREA_CADDYFILE"
  rm -f "$area_tmp"
  # Copy the fixed Caddyfile into the container and reload — no restart.
  docker cp "$AREA_CADDYFILE" "$AREA_PROXY_CONTAINER:/etc/caddy/Caddyfile" >/dev/null 2>&1 || true
  docker exec "$AREA_PROXY_CONTAINER" caddy reload --config /etc/caddy/Caddyfile --adapter caddyfile >/dev/null 2>&1 || \
    docker compose -f compose.area-management.yaml up -d --force-recreate area-management-proxy >/dev/null 2>&1 || true
  printf 'AREA_PROXY=PASS upstream=%s\n' "$ACTIVE_ALIAS"
fi

# --- 5. Verify public health ------------------------------------------------

printf '\n==> 5/6 — Verify public health\n'
public_url="${CROWDRELAY_PUBLIC_BASE_URL:-https://signal-api.virya.music}"
for endpoint in "health/live" "health/ready"; do
  code="$(curl -sS -o /dev/null -w '%{http_code}' --retry 3 --retry-delay 1 --retry-all-errors \
    --connect-timeout 3 --max-time 15 "${public_url%/}/v1/${endpoint}")"
  [[ "$code" == "200" ]] || fail "public health check failed: ${endpoint} -> ${code}"
  printf 'PUBLIC_%s=PASS code=%s\n' "${endpoint^^}" "$code"
done

# Smoke tests
for path in "public/cities?limit=100" "public/events?limit=50"; do
  code="$(curl -sS -o /dev/null -w '%{http_code}' --retry 3 --retry-delay 1 --retry-all-errors \
    --connect-timeout 3 --max-time 15 "${public_url%/}/v1/${path}")"
  [[ "$code" == "200" ]] || fail "public smoke failed: ${path} -> ${code}"
done
printf 'PUBLIC_SMOKE=PASS\n'

# Verify public meta matches target (blocking — stale edge or CDN must be caught)
# The edge Caddy was restarted above, so DNS should re-resolve immediately.
# But CDN/edge caches may take longer to expire. Poll for up to 180 seconds
# (36 attempts × 5s) to give ample time for cache expiry.
public_meta=""
actual=""
for meta_attempt in $(seq 1 36); do
  public_meta="$(curl -sS --connect-timeout 3 --max-time 10 "${public_url%/}/v1/meta" 2>/dev/null || true)"
  if [[ -n "$public_meta" ]]; then
    actual="$(printf '%s' "$public_meta" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("gitSha",""))' 2>/dev/null || true)"
    if [[ "$actual" == "$TARGET" ]]; then
      break
    fi
  fi
  sleep 5
done
if [[ "$actual" == "$TARGET" ]]; then
  printf 'PUBLIC_META=PASS gitSha=%s\n' "$actual"
else
  fail "public meta gitSha mismatch after 180s: got=${actual:-unavailable} expected=$TARGET"
fi

# --- 6. Stop old containers, finalize ---------------------------------------

printf '\n==> 6/6 — Stop old containers, finalize\n'
docker stop --time 30 "$CURRENT_API" "$CURRENT_WORKER" >/dev/null 2>&1 || true
docker rm "$CURRENT_API" "$CURRENT_WORKER" >/dev/null 2>&1 || true

# Update the pin to the new SHA
sed -i "s|^CROWDRELAY_IMAGE_SHA=.*|CROWDRELAY_IMAGE_SHA=\"${TARGET}\"|" .crowdrelay.local.sh
sed -i "s|^CROWDRELAY_IMAGE_TAG=.*|CROWDRELAY_IMAGE_TAG=\"sha-\${CROWDRELAY_IMAGE_SHA}\"|" .crowdrelay.local.sh

# Clean up rollback temp file
rm -f "$CADDY_BACKUP"

trap - ERR INT TERM HUP

printf '\nBLUEGREEN_DEPLOY=PASS sha=%s cutover=zero-downtime old=%s stopped new=%s active\n' "$TARGET" "$CURRENT_API" "$NEW_API"
