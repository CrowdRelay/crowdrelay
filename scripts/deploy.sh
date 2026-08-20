#!/usr/bin/env bash
set -Eeuo pipefail
umask 077

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
TARGET="${1:-}"
WAIT_SECONDS="${CROWDRELAY_DEPLOY_WAIT_SECONDS:-3600}"
POLL_SECONDS="${CROWDRELAY_DEPLOY_POLL_SECONDS:-3}"
CONTROL_PLANE_HOST="${CROWDRELAY_CONTROL_PLANE_HOST:-virya-home}"
CANONICAL="$ROOT_DIR/scripts/deploy-production-safe.sh"

fail() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

require() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

for command in git gh ssh bash; do require "$command"; done
[[ "$WAIT_SECONDS" =~ ^[1-9][0-9]*$ ]] || fail 'CROWDRELAY_DEPLOY_WAIT_SECONDS must be a positive integer'
[[ "$POLL_SECONDS" =~ ^[1-9][0-9]*$ ]] || fail 'CROWDRELAY_DEPLOY_POLL_SECONDS must be a positive integer'

cd "$ROOT_DIR"
[[ -x "$CANONICAL" ]] || fail "canonical deploy is missing or not executable: $CANONICAL"
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
  local workflow="$1" label="$2" deadline run_id
  deadline=$((SECONDS + WAIT_SECONDS))
  run_id=""
  printf '==> Waiting for %s for %s\n' "$label" "$TARGET"
  while (( SECONDS < deadline )); do
    run_id="$(gh run list --repo "$REPO" --workflow "$workflow" --branch main --commit "$TARGET" --limit 1 --json databaseId --jq '.[0].databaseId // empty' 2>/dev/null || true)"
    if [[ -n "$run_id" ]]; then
      printf '%s_RUN=%s\n' "$label" "$run_id"
      gh run watch "$run_id" --repo "$REPO" --exit-status
      printf '%s=PASS sha=%s\n' "$label" "$TARGET"
      return 0
    fi
    sleep "$POLL_SECONDS"
  done
  fail "timed out waiting for $label for $TARGET"
}

control_plane_tunnel_fingerprint() {
  ssh -T "$CONTROL_PLANE_HOST" sudo bash -s <<'REMOTE'
set -Eeuo pipefail
tunnel="crowdrelay-control-plane-virya-area-tunnel-1"
status="$(docker inspect "$tunnel" --format '{{.State.Status}}' 2>/dev/null || true)"
[[ "$status" == "running" ]] || { echo "ERROR: Control Plane tunnel is not running: $status" >&2; exit 1; }
docker inspect "$tunnel" --format '{{.Id}}|{{.State.StartedAt}}|{{.RestartCount}}|{{.State.Status}}'
REMOTE
}

wait_for_workflow "CI" "CI"
wait_for_workflow "Publish container images" "IMAGES"

[[ "$(git rev-parse HEAD)" == "$TARGET" ]] || fail 'local HEAD moved while waiting for release gates'
[[ -z "$(git status --porcelain --untracked-files=normal)" ]] || fail 'local worktree changed while waiting for release gates'
REMOTE_MAIN="$(git ls-remote origin refs/heads/main | awk '{print $1}')"
[[ "$REMOTE_MAIN" == "$TARGET" ]] || fail "origin/main moved while waiting: remote=$REMOTE_MAIN target=$TARGET"

TUNNEL_BEFORE="$(control_plane_tunnel_fingerprint)"
printf 'CONTROL_PLANE_TUNNEL_BASELINE=PASS fingerprint=%s\n' "$TUNNEL_BEFORE"

set +e
bash "$CANONICAL" "$TARGET"
deploy_status=$?
set -e

TUNNEL_AFTER="$(control_plane_tunnel_fingerprint)" || fail 'Control Plane tunnel is unavailable after CrowdRelay deploy'
[[ "$TUNNEL_AFTER" == "$TUNNEL_BEFORE" ]] || fail "CrowdRelay deploy touched Control Plane tunnel: before=$TUNNEL_BEFORE after=$TUNNEL_AFTER"
printf 'CONTROL_PLANE_TUNNEL_PRESERVATION=PASS unchanged=true\n'

(( deploy_status == 0 )) || exit "$deploy_status"
printf 'MAKE_DEPLOY=PASS repo=crowdrelay sha=%s tunnel=preserved\n' "$TARGET"
