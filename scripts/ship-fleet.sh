#!/usr/bin/env bash
set -Eeuo pipefail

# Unified fleet deploy: deploys all active tenants with a runtime from the Mac.
#
# Today only Virya is externally-owned and has a runtime. The framework is
# here for when provisioner-managed tenants arrive: it reads the tenant list
# from the control plane, orders by canary priority, deploys each, polls
# convergence, and stops on first failure.
#
# Usage:
#   just ship-fleet              # deploy all active tenants
#   just ship-fleet --dry-run    # print the plan, deploy nothing
#   just ship-fleet --allow-contract-migrations  # allow destructive DB migrations
#
# Environment:
#   CROWDRELAY_DEPLOY_HOST (default: virya-crowdrelay)
#   CROWDRELAY_CONTROL_PLANE_HOST (default: virya-crowdrelay — SSHed for API calls)

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
REMOTE="${CROWDRELAY_DEPLOY_HOST:-virya-crowdrelay}"
CP_REMOTE="${CROWDRELAY_CONTROL_PLANE_HOST:-virya-crowdrelay}"
CP_DIR="/srv/crowdrelay-control-plane"
DRY_RUN=false
ALLOW_CONTRACT_MIGRATIONS=false

fail() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

# Fleet-level deploy lock. Each per-tenant deploy.sh also acquires its own lock,
# but the fleet lock covers the whole rollout so FakApp doesn't remediate
# between tenants either.
fleet_lock_acquire() {
  scp -q "$ROOT_DIR/scripts/deploy-lock.sh" "$REMOTE:/usr/local/bin/crowdrelay-deploy-lock.sh" 2>/dev/null || true
  ssh -T "$REMOTE" 'sudo bash /usr/local/bin/crowdrelay-deploy-lock.sh acquire ship-fleet' 2>/dev/null || true
}
fleet_lock_release() {
  ssh -T "$REMOTE" 'sudo bash /usr/local/bin/crowdrelay-deploy-lock.sh release' 2>/dev/null || true
}
trap fleet_lock_release EXIT

for command in git ssh bash python3; do
  command -v "$command" >/dev/null 2>&1 || fail "missing required command: $command"
done

# Parse args
while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run) DRY_RUN=true; shift ;;
    --allow-contract-migrations) ALLOW_CONTRACT_MIGRATIONS=true; shift ;;
    *) fail "unknown argument: $1" ;;
  esac
done

cd "$ROOT_DIR"

# --- Pre-flight: clean worktree on main, HEAD == origin/main ---
[[ -z "$(git status --porcelain --untracked-files=normal)" ]] || fail 'local worktree must be clean'
branch="$(git symbolic-ref --quiet --short HEAD 2>/dev/null || true)"
[[ "$branch" == "main" ]] || fail "must run from main, got=${branch:-detached}"
HEAD_SHA="$(git rev-parse HEAD)"
REMOTE_MAIN="$(git ls-remote origin refs/heads/main | awk '{print $1}')"
[[ "$REMOTE_MAIN" == "$HEAD_SHA" ]] || fail "origin/main mismatch: remote=$REMOTE_MAIN local=$HEAD_SHA"
printf 'PREFLIGHT=PASS sha=%s\n' "$HEAD_SHA"

# --- Read the tenant fleet from the control plane ---
# SSH to the server and curl localhost — the admin token is in the env file
# on the server, and the edge proxy strips/replaces Authorization headers.
fleet_json="$(ssh -T "$CP_REMOTE" bash -s <<'REMOTE_QUERY'
set -euo pipefail
admin_token="$(sudo cat /srv/crowdrelay-control-plane/control-plane.env | sed -n 's/^CONTROL_PLANE_ADMIN_TOKEN=//p')"
curl -fsS -H "Authorization: Bearer ${admin_token}" http://127.0.0.1:8090/api/v1/fleet/status
REMOTE_QUERY
)" || fail 'cannot read fleet status from control plane'

# Parse the fleet: extract tenants with a runtime (deployedSha is non-empty)
# and order them: externally-owned first (Virya), then by slug.
mapfile -t fleet_items < <(printf '%s' "$fleet_json" | python3 -c '
import json, sys
data = json.load(sys.stdin)
items = data.get("items", [])
# Only tenants with a runtime (deployedSha non-empty) are deployable.
deployable = [t for t in items if t.get("deployedSha")]
# Externally-owned first (Virya), then alphabetical by slug.
deployable.sort(key=lambda t: (not t.get("externallyOwned", False), t.get("slug", "")))
for t in deployable:
    print(f"{t[\"slug\"]}\t{t.get(\"displayName\",\"\")}\t{t.get(\"deployedSha\",\"\")}\t{t.get(\"externallyOwned\",False)}")
')

if [[ ${#fleet_items[@]} -eq 0 ]]; then
  printf 'FLEET=EMPTY no tenants with a runtime to deploy\n'
  exit 0
fi

printf 'FLEET=%d tenants with runtime:\n' "${#fleet_items[@]}"
for item in "${fleet_items[@]}"; do
  IFS=$'\t' read -r slug name sha ext <<< "$item"
  printf '  %s (%s) — currently %s, externallyOwned=%s\n' "$slug" "$name" "${sha:-unknown}" "$ext"
done

if $DRY_RUN; then
  printf '\nDRY_RUN=SKIP no mutations performed\n'
  exit 0
fi

# --- Migration gate ---
# Run the migration classifier before any deploy. A destructive migration
# against N tenant databases is N broken databases — refuse by default.
if [[ -f scripts/classify-migrations.py ]]; then
  printf '\n==> Migration classification\n'
  if $ALLOW_CONTRACT_MIGRATIONS; then
    python3 scripts/classify-migrations.py --allow-contract-migrations || fail 'migration classification failed'
  else
    python3 scripts/classify-migrations.py || fail 'migration classification failed (use --allow-contract-migrations to override)'
  fi
  printf 'MIGRATIONS=PASS\n'
else
  printf 'MIGRATIONS=SKIP no classifier script found\n'
fi

# --- Deploy each tenant ---
# Today: only externally-owned tenants (Virya) are deployed via `just ship`.
# Provisioner-managed tenants would be deployed via the control plane API,
# but that path is disabled (403) for non-Virya tenants. When the first
# provisioner-managed tenant with a runtime exists, this loop will need to
# call POST /tenants/{slug}/provisioning/deploy for those tenants.
DEPLOYED_COUNT=0
FAILED_COUNT=0

if ! $DRY_RUN; then
  fleet_lock_acquire
fi

for item in "${fleet_items[@]}"; do
  IFS=$'\t' read -r slug name current_sha ext <<< "$item"
  printf '\n==> Deploying %s (%s)\n' "$slug" "$name"

  if [[ "$ext" == "True" ]]; then
    # Externally-owned: run the CrowdRelay blue-green deploy (just ship).
    # This builds images locally, pushes to GHCR, and SSHes to the server.
    printf '  path=just-ship (externally-owned, blue-green)\n'
    if CROWDRELAY_DEPLOY_IMAGE_SOURCE=local bash "$ROOT_DIR/scripts/deploy.sh"; then
      printf 'DEPLOY=PASS tenant=%s sha=%s\n' "$slug" "$HEAD_SHA"
      ((DEPLOYED_COUNT++))
    else
      printf 'DEPLOY=FAIL tenant=%s sha=%s\n' "$slug" "$HEAD_SHA" >&2
      ((FAILED_COUNT++))
      # Stop on first failure — don't cascade a broken deploy to other tenants.
      fail "deploy failed for $slug — stopping fleet rollout (stop-on-first-failure)"
    fi
  else
    # Provisioner-managed: this path is currently disabled at the API level.
    # When re-enabled, this would call the control plane deploy endpoint.
    printf '  path=provisioner-managed (currently disabled — skipping)\n'
    printf 'DEPLOY=SKIP tenant=%s reason=provisioner-managed-deploy-disabled\n' "$slug"
    continue
  fi

  # --- Health gate: wait for the control plane to report the new SHA ---
  printf '  waiting for runtime convergence...\n'
  deadline=$((SECONDS + 300))
  converged=false
  while (( SECONDS < deadline )); do
    reported_sha="$(ssh -T "$CP_REMOTE" bash -s -- "$slug" <<'REMOTE_CHECK'
set -euo pipefail
slug="$1"
admin_token="$(sudo cat /srv/crowdrelay-control-plane/control-plane.env | sed -n 's/^CONTROL_PLANE_ADMIN_TOKEN=//p')"
curl -fsS -H "Authorization: Bearer ${admin_token}" "http://127.0.0.1:8090/api/v1/tenants/${slug}/runtime" 2>/dev/null \
  | python3 -c 'import json,sys; r=json.load(sys.stdin).get("runtime",{}); print(r.get("deployedSha","") if r else "")' 2>/dev/null || echo ""
REMOTE_CHECK
)" || true
    if [[ "$reported_sha" == "$HEAD_SHA" ]]; then
      converged=true
      break
    fi
    sleep 5
  done

  if $converged; then
    printf 'CONVERGENCE=PASS tenant=%s sha=%s\n' "$slug" "$HEAD_SHA"
  else
    printf 'CONVERGENCE=TIMEOUT tenant=%s expected=%s reported=%s\n' "$slug" "$HEAD_SHA" "${reported_sha:-none}" >&2
    fail "tenant $slug did not converge to $HEAD_SHA within 300s — stopping fleet rollout"
  fi
done

# --- Summary ---
printf '\n==> Fleet deploy summary\n'
printf '  deployed: %d\n' "$DEPLOYED_COUNT"
printf '  failed:   %d\n' "$FAILED_COUNT"

# Final fleet status
printf '\n==> Post-deploy fleet status\n'
ssh -T "$CP_REMOTE" bash -s <<'REMOTE_FINAL'
set -euo pipefail
admin_token="$(sudo cat /srv/crowdrelay-control-plane/control-plane.env | sed -n 's/^CONTROL_PLANE_ADMIN_TOKEN=//p')"
curl -fsS -H "Authorization: Bearer ${admin_token}" http://127.0.0.1:8090/api/v1/fleet/status \
  | python3 -m json.tool
REMOTE_FINAL

# Verify the public API
printf '\n==> Public API verification\n'
if curl -fsS --connect-timeout 5 https://signal-api.virya.music/v1/health/ready 2>/dev/null | grep -q '"ready"'; then
  printf 'PUBLIC_API=PASS\n'
else
  printf 'PUBLIC_API=FAIL — check https://signal-api.virya.music/v1/health/ready\n' >&2
  exit 1
fi

printf '\nFLEET_DEPLOY=PASS sha=%s tenants=%d\n' "$HEAD_SHA" "$DEPLOYED_COUNT"
