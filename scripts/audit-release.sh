#!/usr/bin/env bash
set -Eeuo pipefail
umask 077

# Read-only ecosystem release audit.
#
# Verifies a target SHA is safe to deploy without mutating anything:
#   - CI passed on the target SHA
#   - Image artifacts are available
#   - Pending migrations are expand-only (contract migrations fail hard)
#   - All public endpoints are healthy
#   - Runtime SHA matches the target SHA
#   - Image sizes show no catastrophic regression (> 2x previous)
#
# This script does NOT deploy, does NOT SSH with write commands, does NOT
# scp, and does NOT run deploy scripts. Every SSH command is read-only
# (docker inspect, psql SELECT, curl healthz).
#
# Usage:
#   bash scripts/audit-release.sh [target-sha]
#
# Exit codes:
#   0  — audit passed
#   1  — audit failed (contract migrations, unhealthy endpoint, size regression)
#   2  — configuration or pre-flight error

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
CROWDRELAY_REMOTE="${CROWDRELAY_DEPLOY_HOST:-virya-crowdrelay}"
CROWDRELAY_REPO="${CROWDRELAY_DEPLOY_REMOTE_REPO:-/opt/crowdrelay}"
LEDGERGUARD_REMOTE="${LEDGERGUARD_DEPLOY_HOST:-virya-home}"
PUBLIC_BASE_URL="${CROWDRELAY_PUBLIC_BASE_URL:-https://signal-api.virya.music}"
VIRYA_URL="${VIRYA_PRODUCTION_BASE_URL:-https://virya.music}"
SYNESTHESIA_URL="${SYNESTHESIA_PRODUCTION_BASE_URL:-https://synesthesia.virya.music}"
N8N_URL="${N8N_PRODUCTION_BASE_URL:-https://n8n.virya.music}"
LEDGERGUARD_HEALTH_URL="${LEDGERGUARD_HEALTH_URL:-http://127.0.0.1:8080/healthz}"

TARGET=""

fail() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 2
}

audit_failed() {
  printf 'AUDIT=FAILED reason=%s\n' "$1"
  exit 1
}

require() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

for command in git gh ssh curl python3 docker bash; do require "$command"; done

while [[ $# -gt 0 ]]; do
  case "$1" in
    --*) fail "unknown option: $1" ;;
    *) [[ -z "$TARGET" ]] || fail "target SHA already set: $TARGET"; TARGET="$1"; shift ;;
  esac
done

cd "$ROOT_DIR"

HEAD_SHA="$(git rev-parse HEAD)"
[[ -n "$TARGET" ]] || TARGET="$HEAD_SHA"
[[ "$TARGET" =~ ^[0-9a-f]{40}$ ]] || fail 'target must be a 40-char SHA'

REPO="$(gh repo view --json nameWithOwner --jq .nameWithOwner)"
[[ -n "$REPO" ]] || fail 'cannot resolve GitHub repository'

owner_lower="$(printf '%s' "${REPO%%/*}" | tr '[:upper:]' '[:lower:]')"
REGISTRY="ghcr.io"

# --- Step 1: Wait for CI to pass on the target SHA --------------------------

printf '\n==> 1 — Wait for CrowdRelay CI\n'
ci_status="unknown"
deadline=$((SECONDS + 3600))
while (( SECONDS < deadline )); do
  run_id="$(gh run list --repo "$REPO" --workflow "CI" --branch main --commit "$TARGET" --limit 1 --json databaseId --jq '.[0].databaseId // empty')"
  if [[ -n "$run_id" ]]; then
    printf 'CI_RUN=%s\n' "$run_id"
    gh run watch "$run_id" --repo "$REPO" --exit-status
    ci_status="pass"
    printf 'CI=PASS sha=%s\n' "$TARGET"
    break
  fi
  sleep 3
done
[[ "$ci_status" == "pass" ]] || audit_failed "ci_timeout"

# --- Step 2: Wait for image artifacts ---------------------------------------

printf '\n==> 2 — Wait for image artifacts\n'
artifact_name="crowdrelay-image-digests-${TARGET}"
images_status="unavailable"
deadline=$((SECONDS + 3600))
while (( SECONDS < deadline )); do
  artifact_run="$(gh api -H 'Accept: application/vnd.github+json' \
    "/repos/${REPO}/actions/artifacts?name=${artifact_name}&per_page=100" \
    --jq '[.artifacts[] | select(.expired == false)] | sort_by(.created_at) | reverse | .[0].workflow_run.id // empty')"
  if [[ -n "$artifact_run" ]]; then
    images_status="available"
    printf 'IMAGES=PASS sha=%s artifact=%s\n' "$TARGET" "$artifact_name"
    break
  fi
  sleep 5
done
[[ "$images_status" == "available" ]] || audit_failed "images_timeout"

# --- Step 3: Classify pending migrations (fail hard on contract) ------------

printf '\n==> 3 — Classify pending migrations\n'
migration_result="$(python3 scripts/classify-migrations.py --remote "$CROWDRELAY_REMOTE")"
all_expand="$(printf '%s' "$migration_result" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("all_expand", False))')"
pending_count="$(printf '%s' "$migration_result" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("pending_count", 0))')"
contract_count="$(printf '%s' "$migration_result" | python3 -c 'import json,sys; print(len(json.load(sys.stdin).get("contract",[])))')"
expand_count="$(printf '%s' "$migration_result" | python3 -c 'import json,sys; print(len(json.load(sys.stdin).get("expand",[])))')"

printf 'MIGRATION_CLASSIFY=pending=%s expand=%s contract=%s all_expand=%s\n' \
  "$pending_count" "$expand_count" "$contract_count" "$all_expand"

if [[ "$contract_count" != "0" ]]; then
  printf '%s' "$migration_result" | python3 -c \
    'import json,sys; [print(f"  CONTRACT: {m}") for m in json.load(sys.stdin).get("contract",[])]'
  audit_failed "contract_migrations_pending"
fi

migrations_verdict="expand"

# --- Step 4: Health-check all public endpoints ------------------------------

printf '\n==> 4 — Health-check public endpoints\n'
health_status="pass"

check_http() {
  local name="$1" url="$2"
  local valid_codes="$3"
  local code
  code="$(curl -sS -o /dev/null -w '%{http_code}' --retry 3 --retry-delay 1 --retry-all-errors \
    --connect-timeout 5 --max-time 15 "$url" || echo "000")"
  for valid in $valid_codes; do
    [[ "$code" == "$valid" ]] && { printf 'HEALTH=PASS name=%s code=%s\n' "$name" "$code"; return 0; }
  done
  printf 'HEALTH=FAIL name=%s code=%s\n' "$name" "$code" >&2
  health_status="fail"
}

check_http "crowdrelay-live"   "${PUBLIC_BASE_URL%/}/v1/health/live"            "200"
check_http "crowdrelay-ready"  "${PUBLIC_BASE_URL%/}/v1/health/ready"           "200"
check_http "control-plane"     "${PUBLIC_BASE_URL%/}/v1/control-plane/ops/summary" "200"
check_http "virya"             "$VIRYA_URL"                                     "200 301 302"
check_http "synesthesia"       "$SYNESTHESIA_URL"                               "200 301 302"
check_http "n8n"               "${N8N_URL%/}/healthz"                           "200"

# LedgerGuard runs on a private LAN host — curl its healthz via SSH (read-only).
lg_code="$(ssh -T "$LEDGERGUARD_REMOTE" \
  "curl -sS -o /dev/null -w '%{http_code}' --connect-timeout 5 --max-time 10 '$LEDGERGUARD_HEALTH_URL'" \
  || echo "000")"
if [[ "$lg_code" == "200" ]]; then
  printf 'HEALTH=PASS name=ledgerguard code=%s\n' "$lg_code"
else
  printf 'HEALTH=FAIL name=ledgerguard code=%s\n' "$lg_code" >&2
  health_status="fail"
fi

[[ "$health_status" == "pass" ]] || audit_failed "endpoint_unhealthy"

# --- Step 5: Read runtime SHA from remote containers (read-only) ------------

printf '\n==> 5 — Read runtime SHA\n'
runtime_sha_block="$(ssh -T "$CROWDRELAY_REMOTE" bash -s -- "$CROWDRELAY_REPO" <<'READ_SHA'
set -Eeuo pipefail
repo="$1"
cd "$repo"
for service in api worker; do
  container="crowdrelay-${service}-1"
  if ! docker inspect "$container" >/dev/null 2>&1; then
    container="crowdrelay-${service}-green-1"
  fi
  revision="$(docker inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$container")"
  [[ -n "$revision" ]] || { echo "ERROR: no revision label for $service" >&2; exit 1; }
  printf '%s\n' "$revision"
done
READ_SHA
)"

runtime_api_sha="$(printf '%s' "$runtime_sha_block" | sed -n '1p')"
runtime_worker_sha="$(printf '%s' "$runtime_sha_block" | sed -n '2p')"
printf 'RUNTIME_SHA api=%s worker=%s\n' "$runtime_api_sha" "$runtime_worker_sha"

# --- Step 6: Compare runtime SHA against target SHA -------------------------

printf '\n==> 6 — Compare runtime SHA\n'
if [[ "$runtime_api_sha" == "$TARGET" && "$runtime_worker_sha" == "$TARGET" ]]; then
  printf 'RUNTIME_SHA_MATCH=PASS (already deployed)\n'
  runtime_sha_summary="$TARGET"
else
  printf 'RUNTIME_SHA_DIFF api=%s worker=%s target=%s\n' "$runtime_api_sha" "$runtime_worker_sha" "$TARGET"
  runtime_sha_summary="${runtime_api_sha:0:12}"
fi

# --- Step 7: Check image sizes for catastrophic regression ------------------

printf '\n==> 7 — Check image sizes\n'
size_status="pass"

check_image_size() {
  local component="$1" new_sha="$2" old_sha="$3"
  local new_image="${REGISTRY}/${owner_lower}/crowdrelay-${component}:sha-${new_sha}"
  local old_image="${REGISTRY}/${owner_lower}/crowdrelay-${component}:sha-${old_sha}"

  docker pull --quiet "$new_image" >/dev/null
  new_size="$(docker image inspect --format '{{.Size}}' "$new_image")"

  # The previous image may have been untagged or pruned; skip gracefully
  # rather than failing the whole audit on a missing baseline.
  if ! docker pull --quiet "$old_image" >/dev/null 2>&1; then
    printf 'IMAGE_SIZE=SKIP component=%s (previous image unavailable: %s)\n' "$component" "$old_sha"
    return 0
  fi
  old_size="$(docker image inspect --format '{{.Size}}' "$old_image")"

  printf 'IMAGE_SIZE component=%s new=%s old=%s\n' "$component" "$new_size" "$old_size"

  if (( new_size > 2 * old_size )); then
    printf 'IMAGE_SIZE_CATASTROPHIC component=%s new=%d old=%d\n' \
      "$component" "$new_size" "$old_size" >&2
    size_status="fail"
  fi
}

if [[ "$runtime_api_sha" == "$TARGET" ]]; then
  printf 'IMAGE_SIZE=SKIP (runtime already at target SHA)\n'
else
  check_image_size "api"    "$TARGET" "$runtime_api_sha"
  check_image_size "worker" "$TARGET" "$runtime_worker_sha"
fi

[[ "$size_status" == "pass" ]] || audit_failed "image_size_catastrophic_regression"

# --- Summary ----------------------------------------------------------------

printf '\nAUDIT=PASS sha=%s ci=pass images=available migrations=%s runtime_sha=%s health=pass\n' \
  "$TARGET" "$migrations_verdict" "$runtime_sha_summary"
