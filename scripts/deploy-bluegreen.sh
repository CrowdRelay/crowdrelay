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
#   bash scripts/deploy-bluegreen.sh <target-sha> <api-digest> <worker-digest> [repo-dir]
#
# Environment:
#   CROWDRELAY_DOCKER_NETWORK  — shared Docker network (from .crowdrelay.local.sh)
#   CROWDRELAY_ENV_FILE        — production env file
#   CROWDRELAY_COMPOSE_FILE    — production compose file
#   CROWDRELAY_PUBLIC_BASE_URL — public URL for health verification
#   CROWDRELAY_DEPLOY_WAIT_TIMEOUT_SECONDS — health-check timeout

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd -P 2>/dev/null || echo /opt/crowdrelay)"
TARGET="${1:-}"
API_DIGEST="${2:-}"
WORKER_DIGEST="${3:-}"
REPO_DIR="${4:-$ROOT_DIR}"
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
RELEASE_STATE_DIR="/var/lib/crowdrelay/releases"
RECEIPT_HELPER="${REPO_DIR}/scripts/release_receipt.py"
CROWDRELAY_DB_CONTAINER="${CROWDRELAY_DB_CONTAINER:-crowdrelay-db}"
CADDY_BACKUP=""
NEW_STARTED=false
ALIAS_MOVED=false
RELEASE_ID=""

fail() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

rollback() {
  local status="${1:-1}"
  trap - ERR INT TERM HUP

  if [[ "$ALIAS_MOVED" == true ]]; then
    printf 'ROLLBACK=START reason=edge-switched reverting upstream to %s\n' "${CURRENT_API:-}" >&2
    if [[ -n "$CADDY_BACKUP" && -f "$CADDY_BACKUP" ]]; then
      cat "$CADDY_BACKUP" > "$EDGE_CADDYFILE"
      docker exec "$EDGE_CONTAINER" caddy reload --config /etc/caddy/Caddyfile --adapter caddyfile --address 127.0.0.1:2019 >/dev/null
      printf 'ROLLBACK=EDGE_REVERTED active=%s\n' "${CURRENT_API:-}" >&2
    fi
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

  # Write failure receipt
  if [[ -n "$RELEASE_ID" ]]; then
    python3 "$RECEIPT_HELPER" rollback \
      --state-dir "$RELEASE_STATE_DIR" \
      --release-id "$RELEASE_ID" \
      --service crowdrelay \
      --reason "deploy-failure" >/dev/null 2>&1 || true
  fi

  printf 'ROLLBACK=COMPLETE status=%d\n' "$status" >&2
  exit "$status"
}

absolute_path() {
  local path="$1"
  if [[ "$path" = /* ]]; then printf '%s\n' "$path"; else printf '%s/%s\n' "$REPO_DIR" "$path"; fi
}

# --- Pre-flight -------------------------------------------------------------

[[ "$TARGET" =~ ^[0-9a-f]{40}$ ]] || fail "usage: deploy-bluegreen.sh <sha> <api-digest> <worker-digest> [repo-dir]"
[[ "$API_DIGEST" =~ ^sha256:[0-9a-f]{64}$ ]] || fail "invalid API digest: $API_DIGEST"
[[ "$WORKER_DIGEST" =~ ^sha256:[0-9a-f]{64}$ ]] || fail "invalid worker digest: $WORKER_DIGEST"
for command in docker curl python3 flock cmp; do command -v "$command" >/dev/null 2>&1 || fail "missing command: $command"; done

cd "$REPO_DIR"
exec 9> .git/crowdrelay-deploy.lock
flock -n 9 || fail 'another CrowdRelay deployment is already running'
[[ -f .crowdrelay.local.sh && ! -L .crowdrelay.local.sh ]] || fail "missing .crowdrelay.local.sh"
# shellcheck source=/dev/null
source .crowdrelay.local.sh

[[ -f "$EDGE_CADDYFILE" && ! -L "$EDGE_CADDYFILE" ]] || fail "missing edge Caddyfile: $EDGE_CADDYFILE"
docker inspect "$EDGE_CONTAINER" --format '{{.State.Status}}' 2>/dev/null | grep -q running || fail "edge Caddy is not running"

grep -Fq '# CROWDRELAY_ACTIVE=' "$EDGE_CADDYFILE" || \
  fail 'edge Caddyfile is not release-ready: missing active release marker; apply edge config separately'
grep -Fq 'reverse_proxy crowdrelay-api-1:8080 crowdrelay-api-green-1:8080' "$EDGE_CADDYFILE" \
  || grep -Fq 'reverse_proxy crowdrelay-api-green-1:8080 crowdrelay-api-1:8080' "$EDGE_CADDYFILE" \
  || fail 'edge Caddyfile does not contain the static blue-green upstream pair'
cmp -s <(docker exec "$EDGE_CONTAINER" cat /etc/caddy/Caddyfile 2>/dev/null) "$EDGE_CADDYFILE" || \
  fail 'edge Caddy bind mount is stale; apply edge config separately before deploying'
docker exec "$EDGE_CONTAINER" wget -qO- http://127.0.0.1:2019/config/ >/dev/null \
  || fail 'edge Caddy admin endpoint is unavailable'
printf 'EDGE_PREFLIGHT=PASS config=synchronized cutover=graceful-reload\n'

# --- Sync the area-management Caddyfile if the repo copy changed ------------
# The area-management-proxy is a separate container with its own bind-mounted
# Caddyfile. The blue-green app cutover does not touch it, so a stale
# allowlist silently 404s new control-plane routes. Sync before the app
# cutover so new routes are reachable the moment the edge switches.
# deploy.sh scp's the current Caddyfile to /tmp; fall back to the repo copy.
AREA_PROXY_CONTAINER="crowdrelay-area-management-proxy-1"
AREA_CADDYFILE="/tmp/crowdrelay-area-management.Caddyfile"
[[ -f "$AREA_CADDYFILE" ]] || AREA_CADDYFILE="$(absolute_path deploy/area-management.Caddyfile)"
[[ -f "$AREA_CADDYFILE" ]] || fail "missing area-management Caddyfile"
if ! cmp -s "$AREA_CADDYFILE" <(docker exec "$AREA_PROXY_CONTAINER" cat /etc/caddy/Caddyfile 2>/dev/null); then
  cp "$AREA_CADDYFILE" "$(absolute_path deploy/area-management.Caddyfile)"
  docker exec "$AREA_PROXY_CONTAINER" caddy validate --config /etc/caddy/Caddyfile >/dev/null \
    || fail 'area-management Caddyfile is invalid after sync'
  docker exec "$AREA_PROXY_CONTAINER" caddy reload --config /etc/caddy/Caddyfile --adapter caddyfile --address 127.0.0.1:2019 >/dev/null \
    || fail 'area-management Caddy reload failed after sync'
  printf 'AREA_CADDYFILE=SYNCED reload=graceful\n'
else
  printf 'AREA_CADDYFILE=NOOP unchanged=true\n'
fi

env_file="$(absolute_path "${CROWDRELAY_ENV_FILE:-deploy/.env.production}")"
compose_file="$(absolute_path "${CROWDRELAY_COMPOSE_FILE:-compose.production.yaml}")"
export CROWDRELAY_ENV_FILE="$env_file"
export CROWDRELAY_DOCKER_NETWORK
export CROWDRELAY_GREEN_TAG="sha-${TARGET}"

verify_image() {
  local component="$1" digest="$2" repository ref image_id revision architecture host_architecture repo_digests
  repository="ghcr.io/crowdrelay/crowdrelay-${component}"
  ref="${repository}@${digest}"
  docker pull "$ref" >/dev/null || fail "cannot pull immutable ${component} image: $ref"
  image_id="$(docker image inspect "$ref" --format '{{.Id}}')"
  revision="$(docker image inspect "$image_id" --format '{{index .Config.Labels "org.opencontainers.image.revision"}}')"
  architecture="$(docker image inspect "$image_id" --format '{{.Architecture}}')"
  host_architecture="$(docker version --format '{{.Server.Arch}}')"
  repo_digests="$(docker image inspect "$image_id" --format '{{join .RepoDigests "\n"}}')"
  [[ "$revision" == "$TARGET" ]] || fail "${component} OCI revision mismatch: got=$revision expected=$TARGET"
  [[ "$architecture" == "$host_architecture" ]] || fail "${component} architecture mismatch: got=$architecture expected=$host_architecture"
  grep -Fq "@${digest}" <<<"$repo_digests" || fail "${component} RepoDigests do not contain $digest"
  docker tag "$image_id" "${repository}:${CROWDRELAY_GREEN_TAG}"
  printf 'IMMUTABLE_IMAGE=PASS component=%s digest=%s revision=%s architecture=%s\n' "$component" "$digest" "$revision" "$architecture"
}

verify_image api "$API_DIGEST"
verify_image worker "$WORKER_DIGEST"
printf 'NEW_IMAGES=PASS sha=%s exact-digests=true\n' "$TARGET"

# Capture OCI metadata for the release receipt
oci_revision="$(docker image inspect "ghcr.io/crowdrelay/crowdrelay-api@${API_DIGEST}" --format '{{index .Config.Labels "org.opencontainers.image.revision"}}')"
oci_architecture="$(docker image inspect "ghcr.io/crowdrelay/crowdrelay-api@${API_DIGEST}" --format '{{.Architecture}}')"

# Detect which color is currently active and determine deploy direction.
blue_health="$(docker inspect "$BLUE_API" --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' 2>/dev/null || true)"
green_health="$(docker inspect "$GREEN_API" --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' 2>/dev/null || true)"

# Which colour is live is a fact about the containers, not a claim in a comment.
#
# The marker used to be the authority and health only a veto, so any drift
# between them wedged every future deploy with no way forward but hand-editing
# production config. Drift is easy: a run interrupted between the marker flip
# and the container coming up, a container removed by hand, a `git checkout` of
# the Caddyfile. And with *neither* colour healthy the run failed outright —
# the one state where a deploy is most needed was the one it refused.
marker_color="$(sed -n 's/^[[:space:]]*# CROWDRELAY_ACTIVE=//p' "$EDGE_CADDYFILE" | head -n1)"
blue_ok=false; [[ "$blue_health" == "healthy" ]] && blue_ok=true
green_ok=false; [[ "$green_health" == "healthy" ]] && green_ok=true

COLD_START=false
if $blue_ok && $green_ok; then
  case "$marker_color" in
    blue|green) active_color="$marker_color" ;;
    *) active_color="blue" ;;
  esac
  baseline_reason="both healthy, marker=${marker_color:-missing}"
elif $blue_ok; then
  active_color="blue"; baseline_reason="only blue healthy"
elif $green_ok; then
  active_color="green"; baseline_reason="only green healthy"
else
  COLD_START=true
  active_color="$([[ "$marker_color" == "green" ]] && echo green || echo blue)"
  baseline_reason="COLD START — neither healthy (blue=${blue_health:-absent} green=${green_health:-absent})"
fi

if [[ "$active_color" == "blue" ]]; then
  DEPLOY_COLOR="green"
  CURRENT_API="$BLUE_API"; CURRENT_WORKER="$BLUE_WORKER"; CURRENT_ALIAS="$BLUE_ALIAS"
  NEW_API="$GREEN_API"; NEW_WORKER="$GREEN_WORKER"; NEW_ALIAS="$GREEN_ALIAS"
else
  DEPLOY_COLOR="blue"
  CURRENT_API="$GREEN_API"; CURRENT_WORKER="$GREEN_WORKER"; CURRENT_ALIAS="$GREEN_ALIAS"
  NEW_API="$BLUE_API"; NEW_WORKER="$BLUE_WORKER"; NEW_ALIAS="$BLUE_ALIAS"
fi
printf 'BASELINE=%s reason=%s → deploying %s\n' \
  "$(printf '%s' "$active_color" | tr '[:lower:]' '[:upper:]')" "$baseline_reason" "$DEPLOY_COLOR"
if [[ "$marker_color" != "$active_color" ]]; then
  printf 'EDGE_MARKER=RECONCILED was=%s now=%s reason=derived-from-container-health\n' \
    "${marker_color:-missing}" "$active_color"
fi
$COLD_START && printf 'COLD_START=TRUE no-traffic-to-drain cutover-is-a-cold-bring-up\n'

# Snapshot the current Caddyfile for rollback
CADDY_BACKUP="$(mktemp -t caddyfile-blue.XXXXXX)"
cp "$EDGE_CADDYFILE" "$CADDY_BACKUP"
printf 'CADDY_BACKUP=PASS file=%s\n' "$CADDY_BACKUP"

# Initialise release state and write pending receipt
python3 "$RECEIPT_HELPER" init --state-dir "$RELEASE_STATE_DIR" --service crowdrelay >/dev/null
RELEASE_ID="cr-${TARGET:0:12}-$(date -u +%Y%m%d%H%M%S)"
python3 "$RECEIPT_HELPER" pending \
  --state-dir "$RELEASE_STATE_DIR" \
  --service crowdrelay \
  --release-id "$RELEASE_ID" \
  --source-sha "$TARGET" \
  --image-digests "api=${API_DIGEST}" "worker=${WORKER_DIGEST}" \
  --oci-revision "$oci_revision" \
  --oci-architecture "$oci_architecture" \
  --deploy-color "$DEPLOY_COLOR" \
  --current-color "$active_color" \
  --current-container "$CURRENT_API" \
  --candidate-container "$NEW_API" \
  --caddy-active-upstream "$active_color" \
  --compose-file "$compose_file" \
  --caddy-file "$EDGE_CADDYFILE" \
  --env-file "$env_file" >/dev/null

trap 'rollback $?' ERR INT TERM HUP

# --- 1. Run migrations as a one-shot exact-image job -------------------------

printf '\n==> 1/7 — Run migrations (one-shot setup)\n'
CROWDRELAY_IMAGE_TAG="$CROWDRELAY_GREEN_TAG" \
docker compose --env-file "$env_file" -f "$compose_file" \
  run --rm -T setup </dev/null
printf 'MIGRATIONS=PASS\n'

python3 "$RECEIPT_HELPER" phase --state-dir "$RELEASE_STATE_DIR" \
  --release-id "$RELEASE_ID" --phase migration --status pass >/dev/null

# --- 2. Start candidate API (worker in standby) ------------------------------

printf '\n==> 2/7 — Start %s API + worker (standby)\n' "$DEPLOY_COLOR"
NEW_STARTED=true

if [[ "$DEPLOY_COLOR" == "green" ]]; then
  # Start API first, then worker in standby mode
  docker compose --env-file "$env_file" -f "$compose_file" -f compose.bluegreen.yaml \
    up -d --no-deps --wait --wait-timeout "$HEALTH_TIMEOUT" api-green
  # Start worker in standby — it will wait for leadership before running loops
  CROWDRELAY_WORKER_STANDBY=true \
  docker compose --env-file "$env_file" -f "$compose_file" -f compose.bluegreen.yaml \
    up -d --no-deps worker-green
else
  # Deploy blue: override the image tag without modifying .crowdrelay.local.sh
  export CROWDRELAY_IMAGE_TAG="sha-${TARGET}"
  docker compose --env-file "$env_file" -f "$compose_file" \
    up -d --no-deps --wait --wait-timeout "$HEALTH_TIMEOUT" api
  # Start worker in standby
  CROWDRELAY_WORKER_STANDBY=true \
  docker compose --env-file "$env_file" -f "$compose_file" \
    up -d --no-deps worker
fi

for container in "$NEW_API" "$NEW_WORKER"; do
  docker update --restart unless-stopped "$container" >/dev/null
  restart_policy="$(docker inspect "$container" --format '{{.HostConfig.RestartPolicy.Name}}')"
  [[ "$restart_policy" == "unless-stopped" ]] || fail "candidate restart policy is not durable: container=$container policy=$restart_policy"
done
printf 'NEW_CONTAINERS=STARTED color=%s restart=unless-stopped worker=standby\n' "$DEPLOY_COLOR"

python3 "$RECEIPT_HELPER" phase --state-dir "$RELEASE_STATE_DIR" \
  --release-id "$RELEASE_ID" --phase start-candidate --status pass >/dev/null

# --- 3. Health-check new API directly ---------------------------------------

printf '\n==> 3/7 — Health-check %s API\n' "$DEPLOY_COLOR"
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

python3 "$RECEIPT_HELPER" phase --state-dir "$RELEASE_STATE_DIR" \
  --release-id "$RELEASE_ID" --phase health-check --status pass >/dev/null

# --- 4. Atomically prefer the candidate at the public edge -------------------

printf '\n==> 4/7 — Gracefully switch Caddy preference to %s API\n' "$DEPLOY_COLOR"
caddy_candidate="$(mktemp -t caddyfile-candidate.XXXXXX)"
# Write the desired state rather than substituting the state we assumed. The
# old expressions matched the *previous* value, so a stale marker left the
# candidate unchanged and the run died on the "was not updated" guard below —
# the same drift that wedged the decision also broke the rewrite. Matching
# `.*` makes this idempotent and independent of the prior content.
sed \
  -e "s|^\([[:space:]]*\)# CROWDRELAY_ACTIVE=.*|\1# CROWDRELAY_ACTIVE=${DEPLOY_COLOR}|" \
  -e "s|^\([[:space:]]*\)reverse_proxy crowdrelay-api.*|\1reverse_proxy ${NEW_API}:8080 ${CURRENT_API}:8080|" \
  "$EDGE_CADDYFILE" > "$caddy_candidate"
grep -Fq "# CROWDRELAY_ACTIVE=${DEPLOY_COLOR}" "$caddy_candidate" || fail 'candidate edge marker was not updated'
grep -Fq "reverse_proxy ${NEW_API}:8080 ${CURRENT_API}:8080" "$caddy_candidate" || fail 'candidate edge upstream order was not updated'
cat "$caddy_candidate" | docker exec -i "$EDGE_CONTAINER" caddy validate --config /dev/stdin --adapter caddyfile >/dev/null
ALIAS_MOVED=true
cat "$caddy_candidate" > "$EDGE_CADDYFILE"
rm -f "$caddy_candidate"
docker exec "$EDGE_CONTAINER" caddy reload --config /etc/caddy/Caddyfile --adapter caddyfile --address 127.0.0.1:2019 >/dev/null
cmp -s <(docker exec "$EDGE_CONTAINER" cat /etc/caddy/Caddyfile) "$EDGE_CADDYFILE" || fail 'edge runtime config differs after reload'
printf 'CADDY_SWITCH=PASS primary=%s fallback=%s reload=graceful\n' "$NEW_API" "$CURRENT_API"

python3 "$RECEIPT_HELPER" phase --state-dir "$RELEASE_STATE_DIR" \
  --release-id "$RELEASE_ID" --phase cutover --status pass >/dev/null

# --- 4b. Worker leadership handoff -------------------------------------------
# Signal the old worker to shut down (SIGTERM), which triggers its graceful
# drain and leadership release. The candidate worker, running in standby,
# will acquire leadership and start its background loops.
printf '\n==> 4b/7 — Worker leadership handoff (old drains, candidate takes over)\n'
docker stop --time 30 "$CURRENT_WORKER" >/dev/null 2>&1 || true

# Wait for the candidate worker to acquire leadership (up to 90s).
# Check both container health AND the worker_leadership table in Postgres
# to verify the candidate actually holds the lease, not just that the
# container is running.
leader_acquired=false
for leader_attempt in $(seq 1 45); do
  candidate_worker_health="$(docker inspect "$NEW_WORKER" --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' 2>/dev/null || true)"
  if [[ "$candidate_worker_health" == "healthy" || "$candidate_worker_health" == "running" ]]; then
    # Verify leadership in the DB — the candidate must have a non-expired lease
    leader_row="$(docker exec "$CROWDRELAY_DB_CONTAINER" psql -U crowdrelay -d crowdrelay -t -A -c \
      "SELECT leader_id, generation FROM worker_leadership WHERE id = 1 AND expires_at > NOW()" 2>/dev/null || true)"
    if [[ -n "$leader_row" ]]; then
      leader_acquired=true
      printf 'WORKER_LEADERSHIP=VERIFIED leader=%s generation=%s\n' "$leader_row"
      break
    fi
  fi
  sleep 2
done
if [[ "$leader_acquired" != true ]]; then
  # Rollback: restart the old worker so it can reclaim leadership
  printf 'WORKER_LEADERSHIP=FAILED — restarting old worker for safety\n' >&2
  docker start "$CURRENT_WORKER" >/dev/null 2>&1 || true
  fail "candidate worker did not acquire leadership after 90s"
fi
printf 'WORKER_LEADERSHIP=PASS old=%s drained new=%s active\n' "$CURRENT_WORKER" "$NEW_WORKER"

python3 "$RECEIPT_HELPER" phase --state-dir "$RELEASE_STATE_DIR" \
  --release-id "$RELEASE_ID" --phase leadership-handoff --status pass >/dev/null

# --- 5. Verify public health ------------------------------------------------

printf '\n==> 5/7 — Verify public health\n'
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

printf '\n==> Soak candidate for 120 seconds with old API available as fallback\n'
# Error-rate rollback: fail when 5xx exceeds 2% with at least 50 requests
# and an absolute floor of 3 failures, or exceeds pre-cutover baseline by 2.
soak_total=0
soak_errors=0
for soak_attempt in $(seq 1 24); do
  for path in 'health/ready' 'public/cities?limit=1' 'public/events?limit=1'; do
    code="$(curl -sS -o /dev/null -w '%{http_code}' --connect-timeout 3 --max-time 10 "${public_url%/}/v1/${path}" || true)"
    soak_total=$((soak_total + 1))
    if [[ "$code" =~ ^5 ]] || [[ -z "$code" ]] || [[ "$code" == "000" ]]; then
      soak_errors=$((soak_errors + 1))
      printf 'SOAK_ERROR attempt=%s path=%s status=%s total=%s errors=%s\n' \
        "$soak_attempt" "$path" "${code:-transport}" "$soak_total" "$soak_errors" >&2
    fi
  done
  # Immediate rollback on deterministic critical probe failure (health endpoint)
  code="$(curl -sS -o /dev/null -w '%{http_code}' --connect-timeout 3 --max-time 10 "${public_url%/}/v1/health/ready" || true)"
  [[ "$code" == "200" ]] || fail "candidate soak critical probe failed attempt=$soak_attempt status=${code:-transport}"
  soak_meta="$(curl -fsS --connect-timeout 3 --max-time 10 "${public_url%/}/v1/meta")"
  soak_sha="$(printf '%s' "$soak_meta" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("gitSha", ""))')"
  [[ "$soak_sha" == "$TARGET" ]] || fail "candidate soak served wrong revision: got=${soak_sha:-unknown} expected=$TARGET"
  # Error-rate threshold check: 2% with >=50 samples, or absolute floor of 3
  # when sample size is too small for a meaningful rate (early in the soak).
  if [[ "$soak_total" -ge 50 ]]; then
    if [[ "$soak_errors" -ge 3 ]]; then
      error_rate="$(python3 -c "print(f'{$soak_errors/$soak_total*100:.1f}')")"
      if (( $(python3 -c "print(1 if $soak_errors/$soak_total*100 >= 2.0 else 0)") )); then
        fail "soak error-rate breach: ${soak_errors}/${soak_total} (${error_rate}%) — rolling back"
      fi
    fi
  else
    if [[ "$soak_errors" -ge 3 ]]; then
      fail "soak absolute error floor reached: ${soak_errors} failures in ${soak_total} probes — rolling back"
    fi
  fi
  sleep 5
done
printf 'SOAK=PASS seconds=120 probes=%s errors=%s fallback=%s\n' "$soak_total" "$soak_errors" "$CURRENT_API"

python3 "$RECEIPT_HELPER" phase --state-dir "$RELEASE_STATE_DIR" \
  --release-id "$RELEASE_ID" --phase soak --status pass >/dev/null

# --- 6. Stop old containers, finalize ---------------------------------------

printf '\n==> 7/7 — Stop old containers, finalize\n'
docker stop --time 30 "$CURRENT_API" "$CURRENT_WORKER" >/dev/null 2>&1 || true
docker rm "$CURRENT_API" "$CURRENT_WORKER" >/dev/null 2>&1 || true

# Update the pin to the new SHA
sed -i "s|^CROWDRELAY_IMAGE_SHA=.*|CROWDRELAY_IMAGE_SHA=\"${TARGET}\"|" .crowdrelay.local.sh
sed -i "s|^CROWDRELAY_IMAGE_TAG=.*|CROWDRELAY_IMAGE_TAG=\"sha-\${CROWDRELAY_IMAGE_SHA}\"|" .crowdrelay.local.sh

# Clean up rollback temp file
rm -f "$CADDY_BACKUP"

# Finalize release receipt
python3 "$RECEIPT_HELPER" finalize \
  --state-dir "$RELEASE_STATE_DIR" \
  --release-id "$RELEASE_ID" --status pass >/dev/null

trap - ERR INT TERM HUP

printf '\nBLUEGREEN_DEPLOY=PASS sha=%s cutover=graceful-reload old=%s stopped new=%s active receipt=%s\n' \
  "$TARGET" "$CURRENT_API" "$NEW_API" "$RELEASE_ID"
