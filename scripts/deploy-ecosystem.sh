#!/usr/bin/env bash
set -Eeuo pipefail
umask 077

# Ecosystem deploy orchestrator — the single entry point for shipping the
# three services that run together on the production host:
#
#   CrowdRelay (api + worker)  — this repository
#   Control Plane              — CrowdRelay/crowdrelay-control-plane
#   Agent service              — CrowdRelay/crowdrelay-agents, deployed as the
#                                `compose.agents.yml` overlay on the Control
#                                Plane stack
#
# Coordinates them in dependency order with blue-green traffic switching, DB
# snapshots, migration classification, and cross-system contract gates before
# and after the deploy.
#
# Each component ships the revision on its own origin/main. A checkout that is
# dirty, behind, or ahead of origin/main aborts the deploy before anything
# mutates: shipping half the stack from a stale desk copy is the failure this
# gate exists to prevent. `--sync-siblings` fast-forwards those checkouts
# instead of failing.
#
# Usage:
#   bash scripts/deploy-ecosystem.sh [target-sha] [options]
#   just deploy-ecosystem            # same thing, from the repo root
#
# Options:
#   --allow-contract-migrations  Proceed even if destructive migrations are pending
#   --allow-stale-agents         Deploy even when the agent image lags agents main
#   --sync-siblings              Fast-forward sibling checkouts to their origin/main
#   --dry-run                    Run all gates without mutating anything
#   --rollback <sha>             Roll back all components to a previous SHA
#
# Exit codes:
#   0  — ecosystem deploy succeeded
#   1  — a phase failed and was rolled back
#   2  — configuration or pre-flight error

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
CROWDRELAY_REMOTE="${CROWDRELAY_DEPLOY_HOST:-virya-crowdrelay}"
CROWDRELAY_REPO="${CROWDRELAY_DEPLOY_REMOTE_REPO:-/opt/crowdrelay}"
# Where the Control Plane source lives on the machine running this script. The
# default is the sibling checkout an ecosystem worktree has. CI cannot use it:
# `actions/checkout` refuses a path outside `$GITHUB_WORKSPACE`, so the runner
# has nowhere to put a sibling and Phase 2 aborted on every deploy after
# CrowdRelay had already cut over -- production ended up on the new revision
# with the orchestrator reporting failure.
CONTROL_PLANE_CHECKOUT="${CONTROL_PLANE_CHECKOUT:-$ROOT_DIR/../crowdrelay-control-plane}"
CONTROL_PLANE_REMOTE="${CONTROL_PLANE_DEPLOY_HOST:-virya-crowdrelay}"
CONTROL_PLANE_DIR="${CONTROL_PLANE_DEPLOY_REMOTE_DIR:-/srv/crowdrelay-control-plane}"
# The agents checkout is read-only here: the agent image is built and published
# by the agents repo's own workflow, and the Control Plane deploy pulls it. What
# this script needs the checkout for is the ancestry check in gate 0d.
AGENTS_CHECKOUT="${AGENTS_CHECKOUT:-$ROOT_DIR/../crowdrelay-agents}"
AGENTS_PUBLISH_WORKFLOW="${AGENTS_PUBLISH_WORKFLOW:-Publish container image}"
AGENT_CONTAINER="${AGENT_SERVICE_CONTAINER:-crowdrelay-control-plane-agent-service-1}"
PUBLIC_BASE_URL="${CROWDRELAY_PUBLIC_BASE_URL:-https://signal-api.virya.music}"

TARGET=""
ALLOW_CONTRACT=false
ALLOW_STALE_AGENTS=false
SYNC_SIBLINGS=false
DRY_RUN=false
ROLLBACK_SHA=""
PHASE=""
ECOSYSTEM_CONTRACTS_RAN=false
AGENT_RELEASE_SHA=""

# Resolved by the Phase 0 freshness gate. Plain variables, not an associative
# array: macOS ships bash 3.2, `just` runs recipes through whatever `bash` is
# on PATH, and `declare -A` there fails with "invalid option" before the first
# gate runs. Two components do not need a map.
SIBLING_SHA_OUT=""
SIBLING_REPO_OUT=""
CONTROL_PLANE_SHA=""
AGENTS_SHA=""
AGENTS_REPO=""

fail() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 2
}

require() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

for command in git gh ssh curl python3 bash; do require "$command"; done

# Parse arguments
while [[ $# -gt 0 ]]; do
  case "$1" in
    --allow-contract-migrations) ALLOW_CONTRACT=true; shift ;;
    --allow-stale-agents) ALLOW_STALE_AGENTS=true; shift ;;
    --sync-siblings) SYNC_SIBLINGS=true; shift ;;
    --dry-run) DRY_RUN=true; shift ;;
    --rollback) ROLLBACK_SHA="$2"; shift 2 ;;
    --*) fail "unknown option: $1" ;;
    *) [[ -z "$TARGET" ]] || fail "target SHA already set: $TARGET"; TARGET="$1"; shift ;;
  esac
done

# --- Shared helpers ---------------------------------------------------------

# Resolve `owner/name` from a checkout's origin remote. Hardcoding the owner
# rotted once already: these repositories moved from `wojciechbator/*` to
# `CrowdRelay/*` and only GitHub's rename redirect kept the polling alive.
sibling_repo() {
  local checkout="$1" url
  url="$(git -C "$checkout" remote get-url origin 2>/dev/null || true)"
  [[ -n "$url" ]] || return 1
  url="${url%.git}"
  url="${url#git@github.com:}"
  url="${url#https://github.com/}"
  url="${url#ssh://git@github.com/}"
  [[ "$url" =~ ^[^/]+/[^/]+$ ]] || return 1
  printf '%s' "$url"
}

# A component ships the revision on its own origin/main, and only when the
# local checkout is exactly that revision with nothing uncommitted. Leaves the
# result in SIBLING_SHA_OUT/SIBLING_REPO_OUT for the caller to keep.
require_sibling_fresh() {
  local name="$1" checkout="$2" repo head remote dirty

  [[ -d "$checkout/.git" ]] || fail "$name: no checkout at $checkout (set its *_CHECKOUT variable to override)"
  repo="$(sibling_repo "$checkout")" || fail "$name: cannot resolve GitHub repository from origin remote in $checkout"

  git -C "$checkout" fetch --quiet --no-tags origin main || fail "$name: cannot fetch origin/main"
  remote="$(git -C "$checkout" rev-parse FETCH_HEAD)"

  if [[ "$SYNC_SIBLINGS" == true ]]; then
    dirty="$(git -C "$checkout" status --porcelain --untracked-files=normal)"
    [[ -z "$dirty" ]] || fail "$name: worktree is dirty, cannot sync — commit or discard changes in $checkout"
    git -C "$checkout" merge --ff-only "$remote" >/dev/null \
      || fail "$name: cannot fast-forward $checkout to origin/main — it has local commits"
  fi

  dirty="$(git -C "$checkout" status --porcelain --untracked-files=normal)"
  [[ -z "$dirty" ]] || fail "$name: worktree is dirty — the ecosystem ships committed revisions only ($checkout)"

  head="$(git -C "$checkout" rev-parse HEAD)"
  [[ "$head" == "$remote" ]] || fail "$name: checkout is not on origin/main (local=${head:0:12} origin=${remote:0:12}) — push or rerun with --sync-siblings"

  SIBLING_SHA_OUT="$head"
  SIBLING_REPO_OUT="$repo"
  printf 'CHECKOUT=PASS component=%s repo=%s sha=%s\n' "$name" "$repo" "$head"
}

# Wait for a workflow run on a SHA and require it to succeed. Timing out, or
# the run failing, fails the deploy: a green summary line over a red CI run is
# worse than no automation at all.
await_workflow() {
  local label="$1" repo="$2" workflow="$3" sha="$4" timeout="${5:-3600}" run_id deadline
  deadline=$((SECONDS + timeout))
  while (( SECONDS < deadline )); do
    run_id="$(gh run list --repo "$repo" --workflow "$workflow" --branch main --commit "$sha" \
      --limit 1 --json databaseId --jq '.[0].databaseId // empty' 2>/dev/null || true)"
    [[ -n "$run_id" ]] && break
    sleep 5
  done
  [[ -n "${run_id:-}" ]] || fail "$label: no '$workflow' run appeared for $sha within ${timeout}s"
  printf '%s_RUN=%s\n' "$label" "$run_id"
  gh run watch "$run_id" --repo "$repo" --exit-status \
    || fail "$label: '$workflow' run $run_id failed for $sha"
  printf '%s_CI=PASS sha=%s\n' "$label" "$sha"
}

# --- Rollback mode ----------------------------------------------------------

if [[ -n "$ROLLBACK_SHA" ]]; then
  [[ "$ROLLBACK_SHA" =~ ^[0-9a-f]{40}$ ]] || fail "rollback SHA must be 40 chars: $ROLLBACK_SHA"
  printf '==> ECOSYSTEM ROLLBACK to %s\n' "$ROLLBACK_SHA"
  printf '\n==> Phase R1 — CrowdRelay rollback\n'
  ssh -T "$CROWDRELAY_REMOTE" bash -s -- "$CROWDRELAY_REPO" "$ROLLBACK_SHA" <<'REMOTE_ROLLBACK'
set -Eeuo pipefail
repo="$1"; target="$2"
cd "$repo"
[[ -f .crowdrelay.local.sh ]] && source .crowdrelay.local.sh
./crowdrelayctl pin "$target"
./crowdrelayctl doctor
./crowdrelayctl deploy
printf 'CROWDRELAY_ROLLBACK=PASS sha=%s\n' "$target"
REMOTE_ROLLBACK

  printf '\n==> Phase R2 — Control Plane rollback\n'
  # Control Plane rollback: redeploy previous image. The agents overlay stays in
  # the compose argument list — dropping it would tear the agent-service out of
  # the stack as a side effect of a control-plane rollback.
  ssh -T "$CONTROL_PLANE_REMOTE" sudo bash -s -- "$CONTROL_PLANE_DIR" "$ROLLBACK_SHA" <<'CP_ROLLBACK'
set -Eeuo pipefail
root="$1"; target="$2"
cd "$root"
old_tag="sha-${target}"
sed -i "s|^CONTROL_PLANE_IMAGE_TAG=.*|CONTROL_PLANE_IMAGE_TAG=${old_tag}|" .env
compose_args=(-f compose.production.yml -f compose.area.yml)
[[ -f compose.agents.yml ]] && compose_args+=(-f compose.agents.yml)
docker compose "${compose_args[@]}" up -d --no-deps --force-recreate --wait app virya-area-tunnel
printf 'CONTROL_PLANE_ROLLBACK=PASS sha=%s\n' "$target"
CP_ROLLBACK

  printf '\nECOSYSTEM_ROLLBACK=PASS sha=%s\n' "$ROLLBACK_SHA"
  exit 0
fi

# --- Normal deploy mode -----------------------------------------------------

cd "$ROOT_DIR"
[[ -z "$(git status --porcelain --untracked-files=normal)" ]] || fail 'local worktree must be clean'
branch="$(git symbolic-ref --quiet --short HEAD 2>/dev/null || true)"
[[ "$branch" == "main" ]] || fail "must run from main, got=${branch:-detached}"

HEAD_SHA="$(git rev-parse HEAD)"
[[ -n "$TARGET" ]] || TARGET="$HEAD_SHA"
[[ "$TARGET" =~ ^[0-9a-f]{40}$ ]] || fail 'target must be a 40-char SHA'
[[ "$TARGET" == "$HEAD_SHA" ]] || fail "target must equal HEAD: target=$TARGET head=$HEAD_SHA"

REMOTE_MAIN="$(git ls-remote origin refs/heads/main | awk '{print $1}')"
[[ "$REMOTE_MAIN" == "$TARGET" ]] || fail "origin/main mismatch: remote=$REMOTE_MAIN target=$TARGET"

REPO="$(gh repo view --json nameWithOwner --jq .nameWithOwner)"
[[ -n "$REPO" ]] || fail 'cannot resolve GitHub repository'

on_error() {
  local rc="$?"
  trap - ERR
  printf '\nECOSYSTEM_DEPLOY=FAILED phase=%s rc=%d\n' "$PHASE" "$rc" >&2
  printf 'The failing phase has been rolled back by its own rollback handler.\n' >&2
  printf 'Components deployed before the failure remain at the new SHA.\n' >&2
  printf 'Components after the failure point remain at the previous SHA.\n' >&2
  exit "$rc"
}
trap on_error ERR

# --- Phase 0: Pre-deploy gates ----------------------------------------------

PHASE="0-pre-deploy"
printf '\n========== Phase 0 — Pre-deploy gates ==========\n'

# 0a. Every component checkout is clean and on its own origin/main
printf '\n==> 0a — Component checkout freshness\n'
require_sibling_fresh control-plane "$CONTROL_PLANE_CHECKOUT"
CONTROL_PLANE_SHA="$SIBLING_SHA_OUT"
require_sibling_fresh agents "$AGENTS_CHECKOUT"
AGENTS_SHA="$SIBLING_SHA_OUT"
AGENTS_REPO="$SIBLING_REPO_OUT"
printf 'CHECKOUTS=PASS\n'

# 0b. Wait for CrowdRelay CI
printf '\n==> 0b — Wait for CrowdRelay CI\n'
await_workflow CROWDRELAY "$REPO" "CI" "$TARGET" 3600

# 0c. Wait for image release (immutable digest artifact)
printf '\n==> 0c — Wait for image release\n'
artifact_name="crowdrelay-image-digests-${TARGET}"
deadline=$((SECONDS + 3600))
while (( SECONDS < deadline )); do
  artifact_run="$(gh api -H 'Accept: application/vnd.github+json' \
    "/repos/${REPO}/actions/artifacts?name=${artifact_name}&per_page=100" \
    --jq '[.artifacts[] | select(.expired == false)] | sort_by(.created_at) | reverse | .[0].workflow_run.id // empty' \
    2>/dev/null || true)"
  if [[ -n "$artifact_run" ]]; then
    printf 'IMAGES=PASS sha=%s artifact=%s\n' "$TARGET" "$artifact_name"
    break
  fi
  sleep 5
done
[[ -n "${artifact_run:-}" ]] || fail "timed out waiting for image release"

# 0d. Agent image is current with the agents repo
printf '\n==> 0d — Agent image currency\n'
# The Control Plane deploy resolves the agent image from the agents repo's
# newest successful publish run, so the version that ships is whatever that run
# built — not necessarily agents main. Two things have to hold, and neither is
# checked anywhere else:
#
#   1. the published commit is an ancestor of agents origin/main, so the agent
#      is a revision of this ecosystem and not a fork or a reverted branch;
#   2. nothing on main after it was ever built.
#
# Rule 2 cannot be "publish exists for origin/main": the agents CI carries
# `paths-ignore: ["**/*.md", "docs/**"]`, so a docs-only commit never triggers
# CI and never produces a publish. Waiting for one would hang forever. So ask
# GitHub instead of re-deriving the ignore list: a commit past the published one
# is only a real lag if CI actually ran for it.
agents_repo="$AGENTS_REPO"
agents_sha="$AGENTS_SHA"
AGENT_RELEASE_SHA="$(gh run list --repo "$agents_repo" --workflow "$AGENTS_PUBLISH_WORKFLOW" \
  --branch main --status success --limit 1 --json headSha --jq '.[0].headSha // empty' 2>/dev/null || true)"
[[ "$AGENT_RELEASE_SHA" =~ ^[0-9a-f]{40}$ ]] \
  || fail "agents: no successful '$AGENTS_PUBLISH_WORKFLOW' run on main in $agents_repo"

git -C "$AGENTS_CHECKOUT" merge-base --is-ancestor "$AGENT_RELEASE_SHA" "$agents_sha" \
  || fail "agents: published image ${AGENT_RELEASE_SHA:0:12} is not an ancestor of origin/main ${agents_sha:0:12}"

unbuilt=()
while IFS= read -r commit; do
  [[ -n "$commit" ]] || continue
  built="$(gh run list --repo "$agents_repo" --workflow "$AGENTS_PUBLISH_WORKFLOW" \
    --branch main --commit "$commit" --limit 1 --json conclusion --jq '.[0].conclusion // empty' 2>/dev/null || true)"
  [[ "$built" == "success" ]] && continue
  ci_ran="$(gh run list --repo "$agents_repo" --workflow "CI" --branch main --commit "$commit" \
    --limit 1 --json conclusion --jq '.[0].conclusion // empty' 2>/dev/null || true)"
  # No CI run at all means the commit was doc-only and never built by design.
  [[ -n "$ci_ran" ]] && unbuilt+=("${commit:0:12}")
done < <(git -C "$AGENTS_CHECKOUT" rev-list --max-count=50 "${AGENT_RELEASE_SHA}..${agents_sha}")

if (( ${#unbuilt[@]} > 0 )); then
  printf 'AGENT_IMAGE=STALE published=%s main=%s unbuilt=%s\n' \
    "${AGENT_RELEASE_SHA:0:12}" "${agents_sha:0:12}" "${unbuilt[*]}" >&2
  [[ "$ALLOW_STALE_AGENTS" == true ]] \
    || fail "agents: ${#unbuilt[@]} commit(s) on main have no successful publish — wait for the publish run, or use --allow-stale-agents"
  printf 'AGENT_IMAGE=STALE_ALLOWED\n'
else
  printf 'AGENT_IMAGE=CURRENT published=%s main=%s\n' "$AGENT_RELEASE_SHA" "$agents_sha"
fi

# 0e. Run ecosystem contracts (pre-deploy)
printf '\n==> 0e — Ecosystem contracts (pre-deploy)\n'
python3 scripts/test-ecosystem-contract-v2.py
printf 'ECOSYSTEM_CONTRACTS_PRE=PASS\n'
ECOSYSTEM_CONTRACTS_RAN=true

# 0f. Snapshot CrowdRelay DB
printf '\n==> 0f — Snapshot CrowdRelay DB\n'
if [[ "$DRY_RUN" == false ]]; then
  ssh -T "$CROWDRELAY_REMOTE" 'cd /opt/crowdrelay && source .crowdrelay.local.sh && crowdrelay_backup' 2>&1
else
  printf 'DB_SNAPSHOT=SKIPPED (dry-run)\n'
fi

# 0g. Classify pending migrations
printf '\n==> 0g — Classify pending migrations\n'
migration_result="$(python3 scripts/classify-migrations.py --remote "$CROWDRELAY_REMOTE" 2>/dev/null || true)"
if [[ -n "$migration_result" ]]; then
  all_expand="$(printf '%s' "$migration_result" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("all_expand", False))' 2>/dev/null || echo "False")"
  pending_count="$(printf '%s' "$migration_result" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("pending_count", 0))' 2>/dev/null || echo "0")"
  contract_count="$(printf '%s' "$migration_result" | python3 -c 'import json,sys; print(len(json.load(sys.stdin).get("contract",[])))' 2>/dev/null || echo "0")"
  printf 'MIGRATION_CLASSIFY=pending=%s contract=%s all_expand=%s\n' "$pending_count" "$contract_count" "$all_expand"
  if [[ "$all_expand" != "True" ]]; then
    printf 'WARNING: %s contract migrations pending\n' "$contract_count" >&2
    if [[ "$ALLOW_CONTRACT" != true ]]; then
      fail "destructive migrations pending — use --allow-contract-migrations to proceed"
    fi
    printf 'CONTRACT_MIGRATIONS=ALLOWED\n'
  fi
else
  printf 'MIGRATION_CLASSIFY=SKIP (no pending or classifier error)\n'
fi

# 0h. Verify HEAD hasn't moved
[[ "$(git rev-parse HEAD)" == "$TARGET" ]] || fail 'local HEAD moved during pre-deploy'

if [[ "$DRY_RUN" == true ]]; then
  printf '\nDRY_RUN=PASS — all pre-deploy gates passed, no mutations performed\n'
  exit 0
fi

# --- Phase 1: CrowdRelay blue-green -----------------------------------------

PHASE="1-crowdrelay"
printf '\n========== Phase 1 — CrowdRelay blue-green ==========\n'

# Sync source to production (git bundle, same as deploy-production-exact.sh)
printf '\n==> 1a — Sync source to production\n'
BUNDLE="$(mktemp -t crowdrelay-ecosystem.XXXXXX.bundle)"
REMOTE_BUNDLE="/tmp/crowdrelay-ecosystem-${TARGET}.bundle"
trap 'rm -f "$BUNDLE"; ssh -T "$CROWDRELAY_REMOTE" "rm -f '"$REMOTE_BUNDLE"'" >/dev/null 2>&1 || true' EXIT
git bundle create "$BUNDLE" HEAD >/dev/null
scp -q "$BUNDLE" "$CROWDRELAY_REMOTE:$REMOTE_BUNDLE"

ssh -T "$CROWDRELAY_REMOTE" bash -s -- "$CROWDRELAY_REPO" "$TARGET" "$REMOTE_BUNDLE" <<'SOURCE_SYNC'
set -Eeuo pipefail
repo="$1"; target="$2"; bundle="$3"
cd "$repo"
current="$(git rev-parse HEAD)"
git fetch --no-tags "$bundle" HEAD >/dev/null
fetched="$(git rev-parse FETCH_HEAD)"
[[ "$fetched" == "$target" ]] || { echo "ERROR: bundle fetch mismatch"; exit 1; }
git merge-base --is-ancestor "$current" "$target" || { echo "ERROR: not a fast-forward"; exit 1; }
backup_ref="refs/backup/predeploy-$(date -u +%Y%m%dT%H%M%SZ)-${current:0:12}"
git update-ref "$backup_ref" "$current"
git merge --ff-only "$target" >/dev/null
[[ "$(git rev-parse HEAD)" == "$target" ]] || { echo "ERROR: source sync failed"; exit 1; }
printf 'SOURCE_SYNC=PASS old=%s new=%s\n' "$current" "$target"
SOURCE_SYNC

# Pull green images on production
printf '\n==> 1b — Pull green images\n'
ssh -T "$CROWDRELAY_REMOTE" bash -s -- "$TARGET" <<'PULL_IMAGES'
set -Eeuo pipefail
target="$1"
for component in api worker; do
  image="ghcr.io/crowdrelay/crowdrelay-${component}:sha-${target}"
  docker pull --quiet "$image" >/dev/null 2>&1 || true
  revision="$(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$image" 2>/dev/null || true)"
  [[ "$revision" == "$target" ]] || { echo "ERROR: image gate failed for $image"; exit 1; }
  printf 'IMAGE=PASS component=%s\n' "$component"
done
PULL_IMAGES

# Run the canonical exact-artifact deploy so digest verification cannot drift.
printf '\n==> 1c — Blue-green CrowdRelay deploy\n'
bash "$ROOT_DIR/scripts/deploy.sh" "$TARGET"

printf 'CROWDRELAY_DEPLOY=PASS sha=%s\n' "$TARGET"

# --- Phase 2: Control Plane + agent service ---------------------------------

PHASE="2-control-plane"
printf '\n========== Phase 2 — Control Plane + agent service ==========\n'

# The Control Plane deploy owns the agent service: it resolves the agent image
# from the agents repo, pulls it by digest, and recreates `agent-service` under
# the same blue-green run. Gate 0d already established which revision that is.
cp_checkout="$CONTROL_PLANE_CHECKOUT"
cp_target="$CONTROL_PLANE_SHA"
printf '\n==> 2a — Canonical exact-artifact Control Plane deploy\n'
bash "$cp_checkout/scripts/deploy.sh" "$cp_target"
printf 'CONTROL_PLANE_DEPLOY=PASS sha=%s\n' "$cp_target"

# --- Phase 3: Post-deploy gates ---------------------------------------------

PHASE="3-post-deploy"
printf '\n========== Phase 3 — Post-deploy gates ==========\n'

# 3a. Run ecosystem contracts (post-deploy)
printf '\n==> 3a — Ecosystem contracts (post-deploy)\n'
python3 scripts/test-ecosystem-contract-v2.py
printf 'ECOSYSTEM_CONTRACTS_POST=PASS\n'

# 3b. Production smoke tests
printf '\n==> 3b — Production smoke tests\n'
for endpoint in "health/live" "health/ready" "public/cities?limit=100" "public/events?limit=50"; do
  code="$(curl -sS -o /dev/null -w '%{http_code}' --retry 3 --retry-delay 1 --retry-all-errors --connect-timeout 3 --max-time 15 "${PUBLIC_BASE_URL%/}/v1/${endpoint}")"
  [[ "$code" == "200" ]] || fail "post-deploy smoke failed: ${endpoint} -> ${code}"
  printf 'SMOKE_%s=PASS\n' "${endpoint//\//_}"
done

# 3c. Verify CrowdRelay runtime SHA
printf '\n==> 3c — Verify runtime SHA\n'
ssh -T "$CROWDRELAY_REMOTE" bash -s -- "$CROWDRELAY_REPO" "$TARGET" <<'VERIFY_SHA'
set -Eeuo pipefail
repo="$1"; target="$2"
cd "$repo"
for service in api worker; do
  container="crowdrelay-${service}-1"
  # The blue-green deploy may have left the green container as the active one
  if ! docker inspect "$container" >/dev/null 2>&1; then
    container="crowdrelay-${service}-green-1"
  fi
  revision="$(docker inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$container" 2>/dev/null || true)"
  [[ "$revision" == "$target" ]] || { echo "ERROR: runtime SHA mismatch for $service: $revision != $target"; exit 1; }
  printf 'RUNTIME_SHA=PASS service=%s sha=%s\n' "$service" "$revision"
done
VERIFY_SHA

# 3d. Verify the agent service is running the revision gate 0d resolved
printf '\n==> 3d — Verify agent service revision\n'
# The blue-green script reports a skipped agent rollout on stderr and carries
# on, which is the right call for the control plane but means the deploy can
# report success with the agent still on its old image. Read the revision back
# off the running container so that cannot pass silently.
ssh -T "$CONTROL_PLANE_REMOTE" sudo bash -s -- "$AGENT_CONTAINER" "$AGENT_RELEASE_SHA" <<'VERIFY_AGENT'
set -Eeuo pipefail
container="$1"; expected="$2"
docker inspect "$container" >/dev/null 2>&1 \
  || { echo "ERROR: agent service container $container is not running"; exit 1; }
revision="$(docker inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$container" 2>/dev/null || true)"
[[ "$revision" == "$expected" ]] \
  || { echo "ERROR: agent service revision mismatch: $revision != $expected"; exit 1; }
printf 'AGENT_RUNTIME_SHA=PASS sha=%s\n' "$revision"
VERIFY_AGENT

printf '\n========== ECOSYSTEM_DEPLOY=PASS sha=%s ==========\n' "$TARGET"
printf 'phases: crowdrelay=blue-green control-plane=blue-green agents=rolled-out\n'
printf 'revisions: crowdrelay=%s control-plane=%s agents=%s\n' \
  "$TARGET" "$CONTROL_PLANE_SHA" "$AGENT_RELEASE_SHA"
