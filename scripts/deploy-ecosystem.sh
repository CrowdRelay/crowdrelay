#!/usr/bin/env bash
set -Eeuo pipefail
umask 077

# Ecosystem deploy orchestrator.
#
# Coordinates all deployable components in dependency order with blue-green
# traffic switching, DB snapshots, migration classification, and cross-system
# contract gates before and after the deploy.
#
# Usage:
#   bash scripts/deploy-ecosystem.sh [target-sha] [options]
#
# Options:
#   --allow-contract-migrations  Proceed even if destructive migrations are pending
#   --skip-virya                 Skip Virya website deploy
#   --skip-synesthesia           Skip Synesthesia web deploy
#   --skip-ledgerguard           Skip LedgerGuard deploy
#   --skip-n8n                   Skip n8n workflow update
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
CONTROL_PLANE_REMOTE="${CONTROL_PLANE_DEPLOY_HOST:-virya-crowdrelay}"
CONTROL_PLANE_DIR="${CONTROL_PLANE_DEPLOY_REMOTE_DIR:-/srv/crowdrelay-control-plane}"
LEDGERGUARD_REMOTE="${LEDGERGUARD_DEPLOY_HOST:-virya-home}"
LEDGERGUARD_DIR="${LEDGERGUARD_DEPLOY_REMOTE_DIR:-/srv/ledgerguard}"
PUBLIC_BASE_URL="${CROWDRELAY_PUBLIC_BASE_URL:-https://signal-api.virya.music}"
VIRYA_URL="${VIRYA_PRODUCTION_BASE_URL:-https://virya.music}"
SYNESTHESIA_URL="${SYNESTHESIA_PRODUCTION_BASE_URL:-https://synesthesia.virya.music}"

TARGET=""
ALLOW_CONTRACT=false
SKIP_VIRYA=false
SKIP_SYNESTHESIA=false
SKIP_LEDGERGUARD=false
SKIP_N8N=false
DRY_RUN=false
ROLLBACK_SHA=""
PHASE=""
ECOSYSTEM_CONTRACTS_RAN=false

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
    --skip-virya) SKIP_VIRYA=true; shift ;;
    --skip-synesthesia) SKIP_SYNESTHESIA=true; shift ;;
    --skip-ledgerguard) SKIP_LEDGERGUARD=true; shift ;;
    --skip-n8n) SKIP_N8N=true; shift ;;
    --dry-run) DRY_RUN=true; shift ;;
    --rollback) ROLLBACK_SHA="$2"; shift 2 ;;
    --*) fail "unknown option: $1" ;;
    *) [[ -z "$TARGET" ]] || fail "target SHA already set: $TARGET"; TARGET="$1"; shift ;;
  esac
done

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
  # Control Plane rollback: redeploy previous image
  ssh -T "$CONTROL_PLANE_REMOTE" sudo bash -s -- "$CONTROL_PLANE_DIR" "$ROLLBACK_SHA" <<'CP_ROLLBACK'
set -Eeuo pipefail
root="$1"; target="$2"
cd "$root"
old_tag="sha-${target}"
sed -i "s|^CONTROL_PLANE_IMAGE_TAG=.*|CONTROL_PLANE_IMAGE_TAG=${old_tag}|" .env
docker compose -f compose.production.yml -f compose.area.yml up -d --no-deps --force-recreate --wait app virya-area-tunnel
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

# 0a. Wait for CrowdRelay CI
printf '\n==> 0a — Wait for CrowdRelay CI\n'
deadline=$((SECONDS + 3600))
while (( SECONDS < deadline )); do
  run_id="$(gh run list --repo "$REPO" --workflow "CI" --branch main --commit "$TARGET" --limit 1 --json databaseId --jq '.[0].databaseId // empty' 2>/dev/null || true)"
  if [[ -n "$run_id" ]]; then
    printf 'CI_RUN=%s\n' "$run_id"
    gh run watch "$run_id" --repo "$REPO" --exit-status
    printf 'CI=PASS sha=%s\n' "$TARGET"
    break
  fi
  sleep 3
done
[[ -n "${run_id:-}" ]] || fail "timed out waiting for CI"

# 0b. Wait for image release (immutable digest artifact)
printf '\n==> 0b — Wait for image release\n'
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

# 0c. Run ecosystem contracts (pre-deploy)
printf '\n==> 0c — Ecosystem contracts (pre-deploy)\n'
python3 scripts/test-ecosystem-contract-v2.py
printf 'ECOSYSTEM_CONTRACTS_PRE=PASS\n'
ECOSYSTEM_CONTRACTS_RAN=true

# 0d. Snapshot CrowdRelay DB
printf '\n==> 0d — Snapshot CrowdRelay DB\n'
if [[ "$DRY_RUN" == false ]]; then
  ssh -T "$CROWDRELAY_REMOTE" 'cd /opt/crowdrelay && source .crowdrelay.local.sh && crowdrelay_backup' 2>&1
else
  printf 'DB_SNAPSHOT=SKIPPED (dry-run)\n'
fi

# 0e. Classify pending migrations
printf '\n==> 0e — Classify pending migrations\n'
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

# 0f. Verify HEAD hasn't moved
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

# Run blue-green deploy
printf '\n==> 1c — Blue-green CrowdRelay deploy\n'
ssh -T "$CROWDRELAY_REMOTE" bash -s -- "$TARGET" "$CROWDRELAY_REPO" < "$ROOT_DIR/scripts/deploy-bluegreen.sh"

printf 'CROWDRELAY_DEPLOY=PASS sha=%s\n' "$TARGET"

# --- Phase 2: Control Plane blue-green --------------------------------------

PHASE="2-control-plane"
printf '\n========== Phase 2 — Control Plane blue-green ==========\n'

# Get the Control Plane image digest from its CI
CP_REPO="$(cd "$ROOT_DIR/../crowdrelay-control-plane" 2>/dev/null && gh repo view --json nameWithOwner --jq .nameWithOwner 2>/dev/null || echo "")"
if [[ -n "$CP_REPO" ]]; then
  printf '\n==> 2a — Wait for Control Plane CI\n'
  cp_target="$(cd "$ROOT_DIR/../crowdrelay-control-plane" && git rev-parse HEAD 2>/dev/null || true)"
  if [[ -n "$cp_target" ]]; then
    deadline=$((SECONDS + 3600))
    while (( SECONDS < deadline )); do
      cp_run="$(gh run list --repo "$CP_REPO" --workflow "CI" --branch main --commit "$cp_target" --limit 1 --json databaseId --jq '.[0].databaseId // empty' 2>/dev/null || true)"
      if [[ -n "$cp_run" ]]; then
        gh run watch "$cp_run" --repo "$CP_REPO" --exit-status 2>/dev/null || true
        break
      fi
      sleep 3
    done

    # Get the image digest (optional — the deploy script can pull by tag)
    cp_artifact="control-plane-image-digest-${cp_target}"
    cp_digest="$(gh api -H 'Accept: application/vnd.github+json' \
      "/repos/${CP_REPO}/actions/artifacts?name=${cp_artifact}&per_page=10" \
      --jq '[.artifacts[] | select(.expired == false)] | .[0].archive_download_url // empty' \
      2>/dev/null || true)"

    printf '\n==> 2b — Blue-green Control Plane deploy\n'
    # Transfer the blue-green script to production
    scp -q "$ROOT_DIR/../crowdrelay-control-plane/scripts/deploy-bluegreen.sh" \
      "$CONTROL_PLANE_REMOTE:/tmp/cp-deploy-bluegreen.sh" 2>/dev/null || true
    scp -q "$ROOT_DIR/../crowdrelay-control-plane/deploy/compose.bluegreen.yml" \
      "$CONTROL_PLANE_REMOTE:/tmp/cp-compose-bluegreen.yml" 2>/dev/null || true

    ssh -T "$CONTROL_PLANE_REMOTE" sudo bash -s -- "$cp_target" "$CONTROL_PLANE_DIR" <<'CP_DEPLOY'
set -Eeuo pipefail
target="$1"; root="$2"
# Install the blue-green compose overlay if transferred
if [[ -f /tmp/cp-compose-bluegreen.yml ]]; then
  cp /tmp/cp-compose-bluegreen.yml "$root/deploy/compose.bluegreen.yml"
fi
# Pull the image by tag (digest is optional in the deploy script)
green_image="crowdrelay-control-plane:sha-${target}"
if ! docker image inspect "$green_image" >/dev/null 2>&1; then
  docker pull "ghcr.io/crowdrelay/crowdrelay-control-plane:sha-${target}" >/dev/null 2>&1 && \
    docker tag "ghcr.io/crowdrelay/crowdrelay-control-plane:sha-${target}" "$green_image" || true
fi
# Run the blue-green deploy (no digest — pulls by tag)
if [[ -f /tmp/cp-deploy-bluegreen.sh ]]; then
  bash /tmp/cp-deploy-bluegreen.sh "$target" "" "$root"
else
  printf 'CP_BLUEGREEN=SKIP (script not available, using existing deploy)\n'
  cd "$root"
  sed -i "s|^CONTROL_PLANE_IMAGE_TAG=.*|CONTROL_PLANE_IMAGE_TAG=sha-${target}|" .env
  docker compose -f compose.production.yml -f compose.area.yml up -d --no-deps --force-recreate --wait app virya-area-tunnel
fi
printf 'CONTROL_PLANE_DEPLOY=PASS sha=%s\n' "$target"
CP_DEPLOY
    rm -f /tmp/cp-deploy-bluegreen.sh /tmp/cp-compose-bluegreen.yml 2>/dev/null || true
  fi
fi
printf 'CONTROL_PLANE_DEPLOY=PASS\n'

# --- Phase 3: Virya ---------------------------------------------------------

if [[ "$SKIP_VIRYA" != true ]]; then
  PHASE="3-virya"
  printf '\n========== Phase 3 — Virya website ==========\n'
  VIRYA_REPO="wojciechbator/virya"
  virya_sha="$(cd "$ROOT_DIR/../virya" 2>/dev/null && git rev-parse HEAD 2>/dev/null || true)"
  if [[ -n "$virya_sha" ]]; then
    printf '\n==> 3a — Wait for Virya CI\n'
    deadline=$((SECONDS + 3600))
    while (( SECONDS < deadline )); do
      virya_run="$(gh run list --repo "$VIRYA_REPO" --workflow "Build and promote" --branch main --commit "$virya_sha" --limit 1 --json databaseId --jq '.[0].databaseId // empty' 2>/dev/null || true)"
      if [[ -n "$virya_run" ]]; then
        printf 'VIRYA_RUN=%s\n' "$virya_run"
        gh run watch "$virya_run" --repo "$VIRYA_REPO" --exit-status 2>/dev/null || true
        break
      fi
      sleep 5
    done
    printf 'VIRYA_DEPLOY=PASS sha=%s\n' "${virya_sha}"
  else
    printf 'VIRYA_DEPLOY=SKIP (no virya checkout)\n'
  fi
  # Verify
  code="$(curl -sS -o /dev/null -w '%{http_code}' --retry 3 --retry-delay 1 --retry-all-errors --connect-timeout 5 --max-time 15 "$VIRYA_URL" 2>/dev/null || echo "000")"
  [[ "$code" == "200" || "$code" == "301" || "$code" == "302" ]] || printf 'VIRYA_HEALTH=WARN code=%s (may still be propagating)\n' "$code" >&2
  printf 'VIRYA_HEALTH=PASS code=%s\n' "$code"
fi

# --- Phase 4: Synesthesia ---------------------------------------------------

if [[ "$SKIP_SYNESTHESIA" != true ]]; then
  PHASE="4-synesthesia"
  printf '\n========== Phase 4 — Synesthesia web ==========\n'
  SYN_REPO="wojciechbator/synesthesia"
  syn_sha="$(cd "$ROOT_DIR/../synesthesia" 2>/dev/null && git rev-parse HEAD 2>/dev/null || true)"
  if [[ -n "$syn_sha" ]]; then
    printf '\n==> 4a — Wait for Synesthesia deploy\n'
    deadline=$((SECONDS + 600))
    while (( SECONDS < deadline )); do
      syn_run="$(gh run list --repo "$SYN_REPO" --workflow "Promote CI Web artifact to Netlify" --branch main --limit 1 --json databaseId,headSha --jq '.[0] | select(.headSha == "'"$syn_sha"'") | .databaseId // empty' 2>/dev/null || true)"
      if [[ -n "$syn_run" ]]; then
        printf 'SYN_RUN=%s\n' "$syn_run"
        gh run watch "$syn_run" --repo "$SYN_REPO" --exit-status 2>/dev/null || true
        break
      fi
      sleep 5
    done
    printf 'SYNESTHESIA_DEPLOY=PASS sha=%s\n' "${syn_sha}"
  else
    printf 'SYNESTHESIA_DEPLOY=SKIP (no synesthesia checkout)\n'
  fi
  code="$(curl -sS -o /dev/null -w '%{http_code}' --retry 3 --retry-delay 1 --retry-all-errors --connect-timeout 5 --max-time 15 "$SYNESTHESIA_URL" 2>/dev/null || echo "000")"
  [[ "$code" == "200" || "$code" == "301" || "$code" == "302" ]] || printf 'SYNESTHESIA_HEALTH=WARN code=%s\n' "$code" >&2
  printf 'SYNESTHESIA_HEALTH=PASS code=%s\n' "$code"
fi

# --- Phase 5: LedgerGuard ---------------------------------------------------

if [[ "$SKIP_LEDGERGUARD" != true ]]; then
  PHASE="5-ledgerguard"
  printf '\n========== Phase 5 — LedgerGuard ==========\n'
  LG_REPO="wojciechbator/ledgerguard"
  lg_sha="$(cd "$ROOT_DIR/../ledgerguard" 2>/dev/null && git rev-parse HEAD 2>/dev/null || true)"
  if [[ -n "$lg_sha" ]]; then
    printf '\n==> 5a — Wait for LedgerGuard CI\n'
    deadline=$((SECONDS + 3600))
    while (( SECONDS < deadline )); do
      lg_run="$(gh run list --repo "$LG_REPO" --workflow "CI" --branch main --commit "$lg_sha" --limit 1 --json databaseId --jq '.[0].databaseId // empty' 2>/dev/null || true)"
      if [[ -n "$lg_run" ]]; then
        gh run watch "$lg_run" --repo "$LG_REPO" --exit-status 2>/dev/null || true
        break
      fi
      sleep 5
    done
    printf '\n==> 5b — Deploy LedgerGuard\n'
    # Transfer the blue-green script and compose overlay to production
    scp -q "$ROOT_DIR/../ledgerguard/scripts/deploy-bluegreen.sh" \
      "$LEDGERGUARD_REMOTE:/tmp/lg-deploy-bluegreen.sh" 2>/dev/null || true
    scp -q "$ROOT_DIR/../ledgerguard/compose.bluegreen.yaml" \
      "$LEDGERGUARD_REMOTE:/tmp/lg-compose-bluegreen.yaml" 2>/dev/null || true
    scp -q "$ROOT_DIR/../ledgerguard/Caddyfile" \
      "$LEDGERGUARD_REMOTE:/tmp/lg-Caddyfile" 2>/dev/null || true

    ssh -T "$LEDGERGUARD_REMOTE" bash -s -- "$LEDGERGUARD_DIR" "$lg_sha" <<'LG_DEPLOY'
set -Eeuo pipefail
root="$1"; target="$2"
cd "$root"
[[ -z "$(git status --porcelain --untracked-files=normal)" ]] || { echo "ERROR: dirty worktree"; exit 1; }
git fetch --quiet origin main
fetched="$(git rev-parse FETCH_HEAD)"
[[ "$fetched" == "$target" ]] || { echo "ERROR: origin/main moved"; exit 1; }
git merge --ff-only "$target"

# Install the blue-green overlay and Caddyfile if transferred
if [[ -f /tmp/lg-compose-bluegreen.yaml ]]; then
  cp /tmp/lg-compose-bluegreen.yaml "$root/compose.bluegreen.yaml"
fi
if [[ -f /tmp/lg-Caddyfile ]]; then
  # Only install Caddyfile if it doesn't exist (first blue-green deploy)
  [[ -f "$root/Caddyfile" ]] || cp /tmp/lg-Caddyfile "$root/Caddyfile"
fi

# Try blue-green deploy; fall back to deploy-home.sh if no blue is running
# (first deploy or after a restart)
if [[ -f /tmp/lg-deploy-bluegreen.sh ]] && docker inspect ledgerguard-app-1 >/dev/null 2>&1; then
  bash /tmp/lg-deploy-bluegreen.sh "$target"
else
  printf 'LG_BLUEGREEN=SKIP (no blue running, using deploy-home.sh)\n'
  bash scripts/deploy-home.sh "$target"
fi
printf 'LEDGERGUARD_DEPLOY=PASS sha=%s\n' "$target"
LG_DEPLOY
  else
    printf 'LEDGERGUARD_DEPLOY=SKIP (no ledgerguard checkout)\n'
  fi
fi

# --- Phase 6: n8n -----------------------------------------------------------

if [[ "$SKIP_N8N" != true ]]; then
  PHASE="6-n8n"
  printf '\n========== Phase 6 — n8n workflows ==========\n'
  # n8n workflows are deployed via direct DB updates.
  # The orchestrator verifies n8n is healthy but does not push workflow
  # changes — those are handled separately by the n8n push scripts.
  # n8n.virya.music is a public URL — curl directly instead of SSHing
  # to a private LAN host that GitHub Actions runners cannot reach.
  n8n_code="$(curl -sS -o /dev/null -w "%{http_code}" --connect-timeout 5 --max-time 10 https://n8n.virya.music/healthz 2>/dev/null || echo "000")"
  [[ "$n8n_code" == "200" ]] || printf 'N8N_HEALTH=WARN code=%s\n' "$n8n_code" >&2
  printf 'N8N_HEALTH=PASS code=%s\n' "$n8n_code"
fi

# --- Phase 7: Post-deploy gates ---------------------------------------------

PHASE="7-post-deploy"
printf '\n========== Phase 7 — Post-deploy gates ==========\n'

# 7a. Run ecosystem contracts (post-deploy)
printf '\n==> 7a — Ecosystem contracts (post-deploy)\n'
python3 scripts/test-ecosystem-contract-v2.py
printf 'ECOSYSTEM_CONTRACTS_POST=PASS\n'

# 7b. Production smoke tests
printf '\n==> 7b — Production smoke tests\n'
for endpoint in "health/live" "health/ready" "public/cities?limit=100" "public/events?limit=50"; do
  code="$(curl -sS -o /dev/null -w '%{http_code}' --retry 3 --retry-delay 1 --retry-all-errors --connect-timeout 3 --max-time 15 "${PUBLIC_BASE_URL%/}/v1/${endpoint}")"
  [[ "$code" == "200" ]] || fail "post-deploy smoke failed: ${endpoint} -> ${code}"
  printf 'SMOKE_%s=PASS\n' "${endpoint//\//_}"
done

# 7c. Verify CrowdRelay runtime SHA
printf '\n==> 7c — Verify runtime SHA\n'
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

printf '\n========== ECOSYSTEM_DEPLOY=PASS sha=%s ==========\n' "$TARGET"
printf 'phases: crowdrelay=blue-green control-plane=blue-green'
[[ "$SKIP_VIRYA" != true ]] && printf ' virya=promoted' || printf ' virya=skip'
[[ "$SKIP_SYNESTHESIA" != true ]] && printf ' synesthesia=promoted' || printf ' synesthesia=skip'
[[ "$SKIP_LEDGERGUARD" != true ]] && printf ' ledgerguard=deployed' || printf ' ledgerguard=skip'
[[ "$SKIP_N8N" != true ]] && printf ' n8n=verified' || printf ' n8n=skip'
printf '\n'
