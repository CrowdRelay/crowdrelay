#!/usr/bin/env bash
# Deploy the crowdrelay n8n bridge with SQLite snapshot and rollback.
#
# Builds (or pulls) the dedicated bridge image, snapshots the SQLite volume,
# starts the new container, and health-checks. If the health check fails the
# previous image is restored and the SQLite snapshot is rolled back so no
# claims are lost.
#
# Usage:
#   scripts/deploy-bridge.sh [tag]    # deploy (build if no tag given)
#   scripts/deploy-bridge.sh rollback # restore previous image + SQLite snapshot
set -Eeuo pipefail

EDGE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$EDGE_DIR"

COMPOSE_FILE="compose.edge.yaml"
PROJECT="virya-edge"
VOLUME="${PROJECT}_bridge_state"
HEALTH_URL="http://127.0.0.1:8080/health"
CONTAINER="virya-crowdrelay-n8n-bridge"

fail() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
step() { printf '==> %s\n' "$*"; }

snapshot_sqlite() {
  local stamp
  stamp="$(date +%Y%m%dT%H%M%SZ)"
  local snap="/tmp/bridge-sqlite-${stamp}"
  step "Snapshotting SQLite volume to $snap"
  docker run --rm -v "${VOLUME}:/data:ro" -v "$snap:/backup" alpine \
    sh -c 'cp -a /data/. /backup/' 2>/dev/null || true
  printf '%s' "$snap"
}

restore_sqlite() {
  local snap="$1"
  [[ -d "$snap" ]] || fail "snapshot $snap not found"
  step "Restoring SQLite volume from $snap"
  docker run --rm -v "${VOLUME}:/data" -v "$snap:/backup" alpine \
    sh -c 'rm -rf /data/* /data/.* 2>/dev/null; cp -a /backup/. /data/'
}

health_check() {
  step "Health-checking bridge"
  local i
  for i in 1 2 3 4 5; do
    if curl -fsS --max-time 5 "$HEALTH_URL" >/dev/null 2>&1; then
      printf 'HEALTH=PASS\n'
      return 0
    fi
    sleep 2
  done
  printf 'HEALTH=FAIL\n' >&2
  return 1
}

if [[ "${1:-}" == "rollback" ]]; then
  step "Rolling back bridge"
  PREV_TAG="${BRIDGE_PREV_TAG:-latest}"
  latest_snap="$(ls -1dt /tmp/bridge-sqlite-* 2>/dev/null | head -1 || true)"
  [[ -n "$latest_snap" ]] || fail "no SQLite snapshot found in /tmp/bridge-sqlite-*"
  restore_sqlite "$latest_snap"
  BRIDGE_IMAGE_TAG="$PREV_TAG" docker compose -f "$COMPOSE_FILE" -p "$PROJECT" up -d --no-build crowdrelay-n8n-bridge
  health_check || fail "rollback health check failed — inspect $CONTAINER"
  exit 0
fi

TAG="${1:-}"
SNAP="$(snapshot_sqlite)"

if [[ -n "$TAG" ]]; then
  step "Pulling bridge image ghcr.io/crowdrelay/bridge:$TAG"
  docker pull "ghcr.io/crowdrelay/bridge:$TAG"
  export BRIDGE_IMAGE_TAG="$TAG"
  docker compose -f "$COMPOSE_FILE" -p "$PROJECT" up -d --no-build crowdrelay-n8n-bridge
else
  step "Building bridge image from bridge.Dockerfile"
  docker compose -f "$COMPOSE_FILE" -p "$PROJECT" up -d --build crowdrelay-n8n-bridge
fi

if ! health_check; then
  step "Health check failed — rolling back"
  docker logs "$CONTAINER" --tail 20 2>&1 || true
  restore_sqlite "$SNAP"
  docker compose -f "$COMPOSE_FILE" -p "$PROJECT" up -d --no-build crowdrelay-n8n-bridge
  fail "deploy failed, restored previous state from $SNAP"
fi

step "Recording previous tag for rollback"
docker inspect "$CONTAINER" --format '{{index .Config.Image}}' > /tmp/bridge-previous-image.txt 2>/dev/null || true

printf '==> Done. Bridge healthy. Snapshot at %s\n' "$SNAP"
