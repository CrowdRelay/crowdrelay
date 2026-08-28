#!/usr/bin/env bash
set -Eeuo pipefail
umask 077

# Blue-green CrowdRelay deploy with zero-downtime Caddy cutover.
#
# This script runs ON the production host (virya-crowdrelay) via SSH.
# It starts green api+worker containers alongside the current blue ones,
# health-checks the green API directly, runs migrations via setup, switches
# the edge Caddy upstream to green, verifies public health, then stops blue.
#
# On any failure it reverts the Caddy upstream to blue and stops green,
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

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
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
CADDY_BACKUP=""
MUTATED=false
GREEN_STARTED=false
CADDY_SWITCHED=false

fail() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

rollback() {
  local status="${1:-1}"
  trap - ERR INT TERM HUP

  if [[ "$CADDY_SWITCHED" == true ]]; then
    printf 'ROLLBACK=START reason=caddy-switched reverting upstream to %s\n' "$BLUE_ALIAS" >&2
    if [[ -n "$CADDY_BACKUP" && -f "$CADDY_BACKUP" ]]; then
      cp "$CADDY_BACKUP" "$EDGE_CADDYFILE"
      docker exec "$EDGE_CONTAINER" caddy reload --config /etc/caddy/Caddyfile --force >/dev/null 2>&1 || true
      printf 'ROLLBACK=CADDY_REVERTED upstream=%s\n' "$BLUE_ALIAS" >&2
    fi
  fi

  if [[ "$GREEN_STARTED" == true ]]; then
    printf 'ROLLBACK=STOPPING_GREEN\n' >&2
    cd "$REPO_DIR"
    source .crowdrelay.local.sh 2>/dev/null || true
    local env_file compose_file
    env_file="$(absolute_path "${CROWDRELAY_ENV_FILE:-deploy/.env.production}")"
    compose_file="$(absolute_path "${CROWDRELAY_COMPOSE_FILE:-compose.production.yaml}")"
    docker compose --env-file "$env_file" -f "$compose_file" -f compose.bluegreen.yaml \
      stop api-green worker-green >/dev/null 2>&1 || true
    docker compose --env-file "$env_file" -f "$compose_file" -f compose.bluegreen.yaml \
      rm -f api-green worker-green >/dev/null 2>&1 || true
    printf 'ROLLBACK=GREEN_STOPPED\n' >&2
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

env_file="$(absolute_path "${CROWDRELAY_ENV_FILE:-deploy/.env.production}")"
compose_file="$(absolute_path "${CROWDRELAY_COMPOSE_FILE:-compose.production.yaml}")"
export CROWDRELAY_ENV_FILE="$env_file"
export CROWDRELAY_DOCKER_NETWORK
export CROWDRELAY_GREEN_TAG="sha-${TARGET}"

# Verify the green images are available
for component in api worker; do
  image="ghcr.io/crowdrelay/crowdrelay-${component}:${CROWDRELAY_GREEN_TAG}"
  revision="$(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$image" 2>/dev/null || true)"
  [[ "$revision" == "$TARGET" ]] || fail "green image not available or revision mismatch: $image (got=${revision})"
done
printf 'GREEN_IMAGES=PASS sha=%s\n' "$TARGET"

# Verify blue is currently running and healthy
blue_health="$(docker inspect "$BLUE_API" --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' 2>/dev/null || true)"
[[ "$blue_health" == "healthy" || "$blue_health" == "running" ]] || fail "blue API is not healthy: $blue_health"
printf 'BLUE_BASELINE=PASS health=%s\n' "$blue_health"

# Snapshot the current Caddyfile for rollback
CADDY_BACKUP="$(mktemp -t caddyfile-blue.XXXXXX)"
cp "$EDGE_CADDYFILE" "$CADDY_BACKUP"
printf 'CADDY_BACKUP=PASS file=%s\n' "$CADDY_BACKUP"

trap 'rollback $?' ERR INT TERM HUP

# --- 1. Start green containers ----------------------------------------------

printf '\n==> 1/6 — Start green api+worker\n'
MUTATED=true
GREEN_STARTED=true

docker compose --env-file "$env_file" -f "$compose_file" -f compose.bluegreen.yaml \
  up -d --no-deps --wait --wait-timeout "$HEALTH_TIMEOUT" api-green worker-green

printf 'GREEN_CONTAINERS=STARTED\n'

# --- 2. Health-check green API directly -------------------------------------

printf '\n==> 2/6 — Health-check green API\n'
green_health=""
for attempt in $(seq 1 30); do
  green_health="$(docker inspect "$GREEN_API" --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' 2>/dev/null || true)"
  if [[ "$green_health" == "healthy" ]]; then
    break
  fi
  sleep 2
done
[[ "$green_health" == "healthy" ]] || fail "green API did not become healthy: $green_health"

# Direct health check via network alias (from within the Docker network)
docker run --rm --network "$CROWDRELAY_DOCKER_NETWORK" curlimages/curl:8.12.0 \
  --fail --silent --show-error --connect-timeout 3 --max-time 10 \
  "http://${GREEN_ALIAS}:8080/v1/health/ready" >/dev/null

# Verify green serves the correct SHA in /v1/meta
green_meta="$(docker run --rm --network "$CROWDRELAY_DOCKER_NETWORK" curlimages/curl:8.12.0 \
  --fail --silent --show-error --connect-timeout 3 --max-time 10 \
  "http://${GREEN_ALIAS}:8080/v1/meta")"
printf '%s' "$green_meta" | python3 -c "
import json, sys
expected = sys.argv[1]
data = json.load(sys.stdin)
actual = data.get('gitSha', '')
if actual != expected:
    raise SystemExit(f'green meta mismatch: got={actual} expected={expected}')
" "$TARGET"

printf 'GREEN_HEALTH=PASS meta_sha=%s\n' "$TARGET"

# --- 3. Run migrations via setup --------------------------------------------

printf '\n==> 3/6 — Run migrations (setup)\n'
docker compose --env-file "$env_file" -f "$compose_file" \
  run --rm -T setup </dev/null
printf 'MIGRATIONS=PASS\n'

# --- 4. Switch edge Caddy to green ------------------------------------------

printf '\n==> 4/6 — Switch edge Caddy upstream to green\n'
# Replace the upstream in the signal-api block only
# The pattern is specific enough to only match the CrowdRelay API block
sed -i "s|reverse_proxy ${BLUE_ALIAS}:8080|reverse_proxy ${GREEN_ALIAS}:8080|" "$EDGE_CADDYFILE"

# Verify the sed actually changed something
grep -Fq "reverse_proxy ${GREEN_ALIAS}:8080" "$EDGE_CADDYFILE" || fail "Caddyfile was not updated to green upstream"
grep -Fq "reverse_proxy ${BLUE_ALIAS}:8080" "$EDGE_CADDYFILE" && fail "Caddyfile still contains blue upstream — ambiguous state"

# Graceful Caddy reload (zero-downtime: in-flight requests complete, new ones go to green)
docker exec "$EDGE_CONTAINER" caddy reload --config /etc/caddy/Caddyfile --force
CADDY_SWITCHED=true
printf 'CADDY_SWITCH=PASS upstream=%s\n' "$GREEN_ALIAS"

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

# Verify public meta matches target (non-blocking — CDN may cache briefly)
public_meta="$(curl -sS --connect-timeout 3 --max-time 10 "${public_url%/}/v1/meta" 2>/dev/null || true)"
if [[ -n "$public_meta" ]]; then
  actual="$(printf '%s' "$public_meta" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("gitSha",""))' 2>/dev/null || true)"
  if [[ "$actual" == "$TARGET" ]]; then
    printf 'PUBLIC_META=PASS gitSha=%s\n' "$actual"
  else
    printf 'PUBLIC_META=STALE observed=%s expected=%s blocking=false\n' "${actual:-unavailable}" "$TARGET" >&2
  fi
fi

# --- 6. Stop blue, finalize -------------------------------------------------

printf '\n==> 6/6 — Stop blue containers, finalize\n'
docker stop "$BLUE_API" "$BLUE_WORKER" >/dev/null 2>&1 || true
docker rm "$BLUE_API" "$BLUE_WORKER" >/dev/null 2>&1 || true

# Update the pin to the new SHA
sed -i "s|^CROWDRELAY_IMAGE_SHA=.*|CROWDRELAY_IMAGE_SHA=\"${TARGET}\"|" .crowdrelay.local.sh
sed -i "s|^CROWDRELAY_IMAGE_TAG=.*|CROWDRELAY_IMAGE_TAG=\"sha-\${CROWDRELAY_IMAGE_SHA}\"|" .crowdrelay.local.sh

# The green containers are now the new blue. Their compose service names are
# api-green-1 and worker-green-1, but the network alias crowdrelay-api-green
# is what Caddy routes to. On the next deploy, the new green will be started
# and the current green (now serving) will become the old blue to stop.
# The Caddyfile already points to crowdrelay-api-green, which is correct.
# When the next deploy starts, it will create a new green and switch Caddy
# to the new green alias, then stop the current one.

# Clean up rollback temp file
rm -f "$CADDY_BACKUP"

trap - ERR INT TERM HUP

printf '\nBLUEGREEN_DEPLOY=PASS sha=%s cutover=zero-downtime blue=stopped green=active\n' "$TARGET"
