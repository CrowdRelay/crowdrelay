#!/usr/bin/env bash
set -Eeuo pipefail
umask 077

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
TARGET="${1:-}"
WAIT_SECONDS="${CROWDRELAY_DEPLOY_WAIT_SECONDS:-3600}"
POLL_SECONDS="${CROWDRELAY_DEPLOY_POLL_SECONDS:-3}"
CONTROL_PLANE_HOST="${CROWDRELAY_CONTROL_PLANE_HOST:-virya-crowdrelay}"
ORACLE="${CROWDRELAY_DEPLOY_HOST:-virya-crowdrelay}"
ORACLE_REPO="${CROWDRELAY_DEPLOY_REMOTE_REPO:-/opt/crowdrelay}"
BLUEGREEN="$ROOT_DIR/scripts/deploy-bluegreen.sh"
# Fallback for bootstrap/recovery when no blue container is running
CANONICAL="$ROOT_DIR/scripts/deploy-production-safe.sh"
IMAGE_RUN_ID=""
CROWDRELAY_API_DIGEST=""
CROWDRELAY_WORKER_DIGEST=""

fail() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

require() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

for command in git gh ssh bash sha256sum; do require "$command"; done
[[ "$WAIT_SECONDS" =~ ^[1-9][0-9]*$ ]] || fail 'CROWDRELAY_DEPLOY_WAIT_SECONDS must be a positive integer'
[[ "$POLL_SECONDS" =~ ^[1-9][0-9]*$ ]] || fail 'CROWDRELAY_DEPLOY_POLL_SECONDS must be a positive integer'

cd "$ROOT_DIR"
[[ -f "$CANONICAL" && ! -L "$CANONICAL" ]] || fail "canonical deploy is missing or unsafe: $CANONICAL"
[[ -f "$BLUEGREEN" && ! -L "$BLUEGREEN" ]] || fail "blue-green deploy is missing or unsafe: $BLUEGREEN"
[[ -z "$(git status --porcelain --untracked-files=normal)" ]] || fail 'local worktree must be clean'
branch="$(git symbolic-ref --quiet --short HEAD 2>/dev/null || true)"
[[ "$branch" == "main" ]] || fail "make deploy must run from main, got=${branch:-detached}"

HEAD_SHA="$(git rev-parse HEAD)"
[[ -n "$TARGET" ]] || TARGET="$HEAD_SHA"
[[ "$TARGET" =~ ^[0-9a-f]{40}$ ]] || fail 'target must be a full lowercase 40-character SHA'
[[ "$TARGET" == "$HEAD_SHA" ]] || fail "target must equal local HEAD: target=$TARGET head=$HEAD_SHA"
REMOTE_MAIN="$(git ls-remote origin refs/heads/main | awk '{print $1}')"
[[ "$REMOTE_MAIN" == "$TARGET" ]] || fail "origin/main mismatch: remote=$REMOTE_MAIN local=$TARGET"
REPO="$(gh repo view --json nameWithOwner --jq .nameWithOwner)"
[[ -n "$REPO" ]] || fail 'cannot resolve GitHub repository'

wait_for_workflow() {
  local workflow="$1" label="$2" deadline run_id last_notice
  deadline=$((SECONDS + WAIT_SECONDS))
  run_id=""
  last_notice=0
  printf '==> Waiting for %s for %s\n' "$label" "$TARGET"
  while (( SECONDS < deadline )); do
    run_id="$(gh run list --repo "$REPO" --workflow "$workflow" --branch main --commit "$TARGET" --limit 1 --json databaseId --jq '.[0].databaseId // empty' 2>/dev/null || true)"
    if [[ -n "$run_id" ]]; then
      printf '%s_RUN=%s\n' "$label" "$run_id"
      gh run watch "$run_id" --repo "$REPO" --exit-status
      printf '%s=PASS sha=%s\n' "$label" "$TARGET"
      return 0
    fi
    if (( SECONDS - last_notice >= 15 )); then
      printf '... still waiting for %s run for %s\n' "$label" "$TARGET"
      last_notice=$SECONDS
    fi
    sleep "$POLL_SECONDS"
  done
  fail "timed out waiting for $label for $TARGET"
}

wait_for_image_release() {
  local deadline artifact_name run_id last_notice run_identity
  deadline=$((SECONDS + WAIT_SECONDS))
  artifact_name="crowdrelay-image-digests-${TARGET}"
  run_id=""
  last_notice=0
  printf '==> Waiting for IMAGES for %s\n' "$TARGET"

  # A workflow_run execution is itself attached to the default branch HEAD,
  # not necessarily to github.event.workflow_run.head_sha. A later CI run from
  # Dependabot can therefore create a skipped Publish run that `gh run list
  # --commit $TARGET --limit 1` mistakes for the release of TARGET. The digest
  # artifact is named with the source CI SHA and is uploaded only after API,
  # worker, and Rekor images were pushed and their immutable digests validated.
  # Gate deployment on that exact artifact instead of on workflow-run metadata.
  while (( SECONDS < deadline )); do
    run_id="$(
      gh api \
        -H 'Accept: application/vnd.github+json' \
        -H 'X-GitHub-Api-Version: 2022-11-28' \
        "/repos/${REPO}/actions/artifacts?name=${artifact_name}&per_page=100" \
        --jq '[.artifacts[] | select(.expired == false)] | sort_by(.created_at) | reverse | .[0].workflow_run.id // empty' \
        2>/dev/null || true
    )"
    if [[ -n "$run_id" ]]; then
      IMAGE_RUN_ID="$run_id"
      run_identity="$(gh run view "$IMAGE_RUN_ID" --repo "$REPO" --json workflowName,headSha,conclusion --jq '[.workflowName,.headSha,.conclusion] | join("|")')"
      [[ "$run_identity" == "Publish container images|${TARGET}|success" ]] || fail "image artifact run identity mismatch: $run_identity"
      printf 'IMAGES_RUN=%s\n' "$IMAGE_RUN_ID"
      printf 'IMAGES_ARTIFACT=%s\n' "$artifact_name"
      printf 'IMAGES=PASS sha=%s\n' "$TARGET"
      return 0
    fi
    if (( SECONDS - last_notice >= 15 )); then
      printf '... still waiting for immutable image digest artifact %s\n' "$artifact_name"
      last_notice=$SECONDS
    fi
    sleep "$POLL_SECONDS"
  done
  fail "timed out waiting for immutable image digest artifact $artifact_name"
}

download_image_manifest() {
  local artifact_name artifact_dir expected_sum actual_sum release_sha
  artifact_name="crowdrelay-image-digests-${TARGET}"
  [[ -n "$IMAGE_RUN_ID" ]] || fail 'image release run was not resolved'
  artifact_dir="$(mktemp -d)"
  if ! gh run download "$IMAGE_RUN_ID" --repo "$REPO" --name "$artifact_name" --dir "$artifact_dir"; then
    rm -rf -- "$artifact_dir"
    fail "cannot download immutable image manifest: $artifact_name"
  fi
  [[ -f "$artifact_dir/images.env" && -f "$artifact_dir/images.env.sha256" ]] || {
    rm -rf -- "$artifact_dir"
    fail 'image digest manifest is incomplete'
  }
  expected_sum="$(awk 'NR==1 {print $1}' "$artifact_dir/images.env.sha256")"
  actual_sum="$(sha256sum "$artifact_dir/images.env" | awk '{print $1}')"
  [[ "$expected_sum" =~ ^[0-9a-f]{64}$ && "$expected_sum" == "$actual_sum" ]] || {
    rm -rf -- "$artifact_dir"
    fail 'image digest manifest checksum failed'
  }
  release_sha="$(sed -n 's/^CROWDRELAY_RELEASE_SHA=//p' "$artifact_dir/images.env")"
  CROWDRELAY_API_DIGEST="$(sed -n 's/^CROWDRELAY_API_DIGEST=//p' "$artifact_dir/images.env")"
  CROWDRELAY_WORKER_DIGEST="$(sed -n 's/^CROWDRELAY_WORKER_DIGEST=//p' "$artifact_dir/images.env")"
  rm -rf -- "$artifact_dir"
  [[ "$release_sha" == "$TARGET" ]] || fail "image manifest SHA mismatch: got=$release_sha expected=$TARGET"
  [[ "$CROWDRELAY_API_DIGEST" =~ ^sha256:[0-9a-f]{64}$ ]] || fail 'invalid API digest in image manifest'
  [[ "$CROWDRELAY_WORKER_DIGEST" =~ ^sha256:[0-9a-f]{64}$ ]] || fail 'invalid worker digest in image manifest'
  printf 'IMAGE_MANIFEST=PASS sha=%s api=%s worker=%s\n' "$TARGET" "$CROWDRELAY_API_DIGEST" "$CROWDRELAY_WORKER_DIGEST"
}

control_plane_tunnel_fingerprint() {
  ssh -T "$CONTROL_PLANE_HOST" sudo bash -s <<'REMOTE'
# Remote body runs as one brace group with stdin detached. bash reads this
# script from ssh stdin; any command that attaches stdin (docker compose
# run/exec) would otherwise swallow the remainder and silently skip it.
{
set -Eeuo pipefail
tunnel="crowdrelay-control-plane-virya-area-tunnel-1"
status="$(docker inspect "$tunnel" --format '{{.State.Status}}' 2>/dev/null || true)"
[[ "$status" == "running" ]] || { echo "ERROR: Control Plane tunnel is not running: $status" >&2; exit 1; }
docker inspect "$tunnel" --format '{{.Id}}|{{.State.StartedAt}}|{{.RestartCount}}|{{.State.Status}}'
} </dev/null
REMOTE
}

recover_exact_runtime_convergence() {
  printf 'RUNTIME_CONVERGENCE_RECOVERY=CHECK sha=%s\n' "$TARGET" >&2
  ssh -T "$ORACLE" bash -s -- "$ORACLE_REPO" "$TARGET" <<'REMOTE_RECOVERY'
# Remote body runs as one brace group with stdin detached. bash reads this
# script from ssh stdin; any command that attaches stdin (docker compose
# run/exec) would otherwise swallow the remainder and silently skip it.
{
set -Eeuo pipefail
repo="$1"
target="$2"
cd "$repo"

fail() {
  printf 'RUNTIME_CONVERGENCE_RECOVERY=REFUSED reason=%s\n' "$*" >&2
  exit 1
}

for command in docker python3 sha256sum; do
  command -v "$command" >/dev/null 2>&1 || fail "missing-$command"
done
[[ "$(git rev-parse HEAD)" == "$target" ]] || fail 'source-head-mismatch'
[[ -z "$(git status --porcelain --untracked-files=normal)" ]] || fail 'dirty-worktree'
[[ -f .crowdrelay.local.sh && ! -L .crowdrelay.local.sh ]] || fail 'local-config-missing'
# shellcheck source=/dev/null
source .crowdrelay.local.sh
[[ "${CROWDRELAY_IMAGE_SHA:-}" == "$target" ]] || fail 'pin-mismatch'

for component in api worker; do
  image="ghcr.io/crowdrelay/crowdrelay-${component}:sha-${target}"
  [[ "$(docker image inspect "$image" --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' 2>/dev/null || true)" == "$target" ]] \
    || fail "target-image-invalid-$component"
done

needs_recreate=false
for service in api worker; do
  container="crowdrelay-${service}-1"
  image_id="$(docker inspect "$container" --format '{{.Image}}' 2>/dev/null || true)"
  [[ -n "$image_id" ]] || fail "runtime-missing-$service"
  revision="$(docker image inspect "$image_id" --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' 2>/dev/null || true)"
  [[ "$revision" =~ ^[0-9a-f]{40}$ ]] || fail "runtime-revision-invalid-$service"
  if [[ "$revision" != "$target" ]]; then
    needs_recreate=true
  fi
done
[[ "$needs_recreate" == true ]] || fail 'runtime-already-exact-failure-is-not-convergence'

absolute_path() {
  local path="$1"
  if [[ "$path" = /* ]]; then printf '%s\n' "$path"; else printf '%s/%s\n' "$PWD" "$path"; fi
}

env_file="$(absolute_path "${CROWDRELAY_ENV_FILE:-deploy/.env.production}")"
compose_file="$(absolute_path "${CROWDRELAY_COMPOSE_FILE:-compose.production.yaml}")"
export CROWDRELAY_ENV_FILE="$env_file"
export CROWDRELAY_BOOTSTRAP_HOST_FILE="$(absolute_path "${CROWDRELAY_BOOTSTRAP_FILE:-deploy/bootstrap.production.json}")"
export CROWDRELAY_WEBHOOK_SECRETS_HOST_FILE="$(absolute_path "${CROWDRELAY_WEBHOOK_SECRETS_FILE:-deploy/webhook-secrets.production.json}")"
export CROWDRELAY_FCM_SERVICE_ACCOUNT_HOST_FILE="$(absolute_path "${CROWDRELAY_FCM_SERVICE_ACCOUNT_FILE:-deploy/secrets/firebase-service-account.json}")"
export CROWDRELAY_DOCKER_NETWORK
export CROWDRELAY_IMAGE_TAG="sha-${target}"
compose_args=(--env-file "$env_file" -f "$compose_file")
if [[ "${CROWDRELAY_AREA_MANAGEMENT_ENABLED:-false}" == "true" ]]; then
  [[ -f compose.area-management.yaml && ! -L compose.area-management.yaml ]] || fail 'area-overlay-missing'
  [[ -f deploy/area-management.Caddyfile && ! -L deploy/area-management.Caddyfile ]] || fail 'area-caddyfile-missing'
  export CROWDRELAY_AREA_MANAGEMENT_CONFIG_SHA256="$(sha256sum deploy/area-management.Caddyfile | awk '{print $1}')"
  compose_args+=(-f compose.area-management.yaml)
fi
compose() { docker compose "${compose_args[@]}" "$@"; }

compose config --format json | python3 -c '
import json, sys
model=json.load(sys.stdin)
target=sys.argv[1]
for service, component in (("api","api"),("worker","worker")):
    image=model["services"][service]["image"]
    expected=f"ghcr.io/crowdrelay/crowdrelay-{component}:sha-{target}"
    if image != expected:
        raise SystemExit(f"effective image mismatch for {service}: {image} != {expected}")
' "$target" || fail 'effective-compose-not-exact'

# setup owns migrations/bootstrap and must complete before either long-running
# service is replaced. Only api+worker are force-recreated; the Oracle
# management proxy and the Home Control Plane tunnel are deliberately excluded.
compose pull api worker setup
compose run --rm -T setup </dev/null
compose up -d --no-deps --force-recreate --wait --wait-timeout "${CROWDRELAY_DEPLOY_WAIT_TIMEOUT_SECONDS:-180}" api worker

for service in api worker; do
  container="crowdrelay-${service}-1"
  configured="$(docker inspect "$container" --format '{{.Config.Image}}')"
  image_id="$(docker inspect "$container" --format '{{.Image}}')"
  revision="$(docker image inspect "$image_id" --format '{{index .Config.Labels "org.opencontainers.image.revision"}}')"
  [[ "$configured" == *":sha-${target}" ]] || fail "post-recovery-tag-mismatch-$service"
  [[ "$revision" == "$target" ]] || fail "post-recovery-revision-mismatch-$service"
done
meta="$(docker exec crowdrelay-api-1 curl -fsS --connect-timeout 2 --max-time 10 http://127.0.0.1:8080/v1/meta)"
printf '%s' "$meta" | python3 -c '
import json, sys
value=json.load(sys.stdin)
expected=sys.argv[1]
if value.get("gitSha") != expected:
    raise SystemExit(f"runtime meta mismatch: {value.get('gitSha')} != {expected}")
' "$target" || fail 'post-recovery-meta-mismatch'
printf 'RUNTIME_CONVERGENCE_RECOVERY=PASS sha=%s services=api,worker proxy=untouched\n' "$target"
} </dev/null
REMOTE_RECOVERY
}

wait_for_workflow "CI" "CI"
wait_for_image_release
download_image_manifest

[[ "$(git rev-parse HEAD)" == "$TARGET" ]] || fail 'local HEAD moved while waiting for release gates'
[[ -z "$(git status --porcelain --untracked-files=normal)" ]] || fail 'local worktree changed while waiting for release gates'
REMOTE_MAIN="$(git ls-remote origin refs/heads/main | awk '{print $1}')"
[[ "$REMOTE_MAIN" == "$TARGET" ]] || fail "origin/main moved while waiting: remote=$REMOTE_MAIN target=$TARGET"

TUNNEL_BEFORE="$(control_plane_tunnel_fingerprint)"
printf 'CONTROL_PLANE_TUNNEL_BASELINE=PASS fingerprint=%s\n' "$TUNNEL_BEFORE"

# --- Deploy via blue-green (zero-downtime) or fallback to force-recreate ---
# Blue-green is the canonical path. If no blue/green container is running
# (first install or recovery), fall back to the force-recreate path.
blue_green_eligible="$(ssh -T "$ORACLE" bash -s -- "$ORACLE_REPO" <<'REMOTE_CHECK'
{
set -euo pipefail
repo="$1"
cd "$repo" 2>/dev/null || exit 0
blue="$(docker inspect crowdrelay-api-1 --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' 2>/dev/null || true)"
green="$(docker inspect crowdrelay-api-green-1 --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' 2>/dev/null || true)"
if [[ "$blue" == "healthy" || "$blue" == "running" || "$green" == "healthy" || "$green" == "running" ]]; then
  echo "eligible"
fi
} </dev/null
REMOTE_CHECK
)"

if [[ "$blue_green_eligible" == "eligible" ]]; then
  printf '\n==> Blue-green deploy (zero-downtime Caddy cutover)\n'
  # Run cross-system source contracts before the blue-green cutover
  python3 scripts/test_area_deploy_contract.py
  python3 scripts/test_control_plane_management_contract.py
  python3 scripts/test_boring_production_deploy_contract.py
  printf 'SOURCE_CONTRACTS=PASS\n'

  # Ship the blue-green script, receipt helper, and area-management Caddyfile
  # to the remote. The Caddyfile is scp'd so the blue-green script can detect
  # drift and reload the area-management proxy without a separate manual step.
  # The receipt helper is scp'd because the remote repo may be stale — the
  # deploy itself is what updates it, and the script runs before that happens.
  scp -q "$ROOT_DIR/scripts/release_receipt.py" \
       "$ORACLE:/tmp/release_receipt.py" \
    || fail "could not copy release_receipt.py to $ORACLE"
  scp -q "$ROOT_DIR/deploy/area-management.Caddyfile" \
       "$ORACLE:/tmp/crowdrelay-area-management.Caddyfile" \
    || fail "could not copy area-management.Caddyfile to $ORACLE"
  ssh -T "$ORACLE" bash -s -- "$TARGET" "$CROWDRELAY_API_DIGEST" "$CROWDRELAY_WORKER_DIGEST" "$ORACLE_REPO" < "$BLUEGREEN"
  deploy_status=$?
else
  printf '\n==> Bootstrap/recovery deploy (force-recreate — no blue/green container running)\n'
  set +e
  bash "$CANONICAL" "$TARGET"
  deploy_status=$?
  set -e

  TUNNEL_AFTER="$(control_plane_tunnel_fingerprint)" || fail 'Control Plane tunnel is unavailable after CrowdRelay deploy'
  [[ "$TUNNEL_AFTER" == "$TUNNEL_BEFORE" ]] || fail "CrowdRelay deploy touched Control Plane tunnel: before=$TUNNEL_BEFORE after=$TUNNEL_AFTER"
  printf 'CONTROL_PLANE_TUNNEL_PRESERVATION=PASS unchanged=true\n'

  if (( deploy_status != 0 )); then
    printf 'CANONICAL_DEPLOY=FAILED status=%d checking-bounded-convergence-recovery=true\n' "$deploy_status" >&2
    RECOVERY_TUNNEL_BEFORE="$(control_plane_tunnel_fingerprint)"
    if recover_exact_runtime_convergence; then
      RECOVERY_TUNNEL_AFTER="$(control_plane_tunnel_fingerprint)" || fail 'Control Plane tunnel is unavailable after convergence recovery'
      [[ "$RECOVERY_TUNNEL_AFTER" == "$RECOVERY_TUNNEL_BEFORE" ]] || fail "runtime convergence recovery touched Control Plane tunnel: before=$RECOVERY_TUNNEL_BEFORE after=$RECOVERY_TUNNEL_AFTER"
      printf 'CONTROL_PLANE_TUNNEL_RECOVERY_PRESERVATION=PASS unchanged=true\n'
      printf '==> Retrying canonical deploy once after exact runtime convergence\n'
      bash "$CANONICAL" "$TARGET"
    else
      exit "$deploy_status"
    fi
  fi
fi

TUNNEL_FINAL="$(control_plane_tunnel_fingerprint)" || fail 'Control Plane tunnel is unavailable at final receipt'
[[ "$TUNNEL_FINAL" == "$TUNNEL_BEFORE" ]] || fail "CrowdRelay deploy changed Control Plane tunnel before final receipt: before=$TUNNEL_BEFORE after=$TUNNEL_FINAL"
printf 'CONTROL_PLANE_TUNNEL_FINAL=PASS unchanged=true\n'
printf 'MAKE_DEPLOY=PASS repo=crowdrelay sha=%s tunnel=preserved exact-runtime=true\n' "$TARGET"
