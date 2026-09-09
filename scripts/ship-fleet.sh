#!/usr/bin/env bash
# One rollout for the whole estate, driven from the developer machine.
#
# There are two deploy substrates and they were never unified:
#
#   Externally-owned tenants (currently just `virya`) run on the shared
#   production stack on virya-crowdrelay. `store::tenant_lifecycle_is_externally
#   _owned` in the Control Plane hardcodes that list. Their deploy is the
#   blue-green cutover in scripts/deploy.sh.
#
#   Provisioner-managed tenants each get their own compose project — own
#   Postgres, own api, own worker — rendered by deploy/provisioner.py in the
#   Control Plane repo from a leased, crash-recoverable job. Their deploy is
#   POST /tenants/{slug}/provisioning/deploy.
#
# Before this script, rolling everything meant running one command for the
# shared stack and then clicking through the Control Plane once per tenant, in
# whatever order you happened to pick, with no halt-on-failure and no way to see
# afterwards which tenant ended up on which revision. This is that, as one
# ordered, interruptible operation.
#
# Deliberately NOT automated on a trigger: no push, no schedule, no webhook.
# Production ships when an operator runs this.
set -Eeuo pipefail
umask 077

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
CONTROL_PLANE_URL="${CROWDRELAY_CONTROL_PLANE_URL:-https://control.crowdrelay.music}"
CONTROL_PLANE_ENV="${CROWDRELAY_CONTROL_PLANE_ENV:-$ROOT_DIR/../crowdrelay-control-plane/.env}"
# Mirrors store::tenant_lifecycle_is_externally_owned. If that function grows a
# slug, this has to grow it too or the new tenant gets deployed twice: once by
# the shared blue-green and again as a provisioner job that cannot serve it.
EXTERNAL_SLUGS="${CROWDRELAY_EXTERNAL_TENANTS:-virya}"
# Rolled first and alone. A tenant that exists to absorb the first failure is
# worth more than a fast rollout.
CANARY_SLUGS="${CROWDRELAY_FLEET_CANARY:-test}"
JOB_TIMEOUT="${CROWDRELAY_FLEET_JOB_TIMEOUT:-900}"
JOB_POLL="${CROWDRELAY_FLEET_JOB_POLL:-5}"

SKIP_SHARED=0
REPORT_ONLY=0
ALLOW_UNHEALTHY=0
ONLY_SLUG=""
TARGET=""

fail() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

require() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

usage() {
  cat <<'USAGE'
Usage: ship-fleet.sh [SHA] [options]

  SHA                Full 40-char lowercase SHA. Defaults to local HEAD.

  --report           Print the fleet revision table and exit. Mutates nothing.
  --skip-shared      Do not deploy the shared stack; roll provisioned tenants only.
  --only SLUG        Roll exactly one provisioned tenant.
  --allow-unhealthy  Roll a tenant whose runtime is not currently healthy.
USAGE
}

while (( $# )); do
  case "$1" in
    --report) REPORT_ONLY=1 ;;
    --skip-shared) SKIP_SHARED=1 ;;
    --allow-unhealthy) ALLOW_UNHEALTHY=1 ;;
    --only)
      shift
      [[ $# -gt 0 ]] || fail '--only requires a slug'
      ONLY_SLUG="$1"
      ;;
    -h|--help) usage; exit 0 ;;
    -*) fail "unknown option: $1" ;;
    *)
      [[ -z "$TARGET" ]] || fail "unexpected argument: $1"
      TARGET="$1"
      ;;
  esac
  shift
done

for command in git curl python3 bash; do require "$command"; done
cd "$ROOT_DIR"

[[ -n "$TARGET" ]] || TARGET="$(git rev-parse HEAD)"
[[ "$TARGET" =~ ^[0-9a-f]{40}$ ]] || fail 'target must be a full lowercase 40-character SHA'

# Read once, never echoed, never passed in argv: curl reads the header from a
# config document on stdin so the token does not appear in `ps`.
resolve_token() {
  if [[ -n "${CONTROL_PLANE_ADMIN_TOKEN:-}" ]]; then
    printf '%s' "$CONTROL_PLANE_ADMIN_TOKEN"
    return 0
  fi
  [[ -r "$CONTROL_PLANE_ENV" ]] \
    || fail "no CONTROL_PLANE_ADMIN_TOKEN in the environment and cannot read $CONTROL_PLANE_ENV"
  local value
  value="$(sed -n 's/^CONTROL_PLANE_ADMIN_TOKEN=//p' "$CONTROL_PLANE_ENV" | head -n1)"
  [[ -n "$value" ]] || fail "CONTROL_PLANE_ADMIN_TOKEN is absent from $CONTROL_PLANE_ENV"
  printf '%s' "$value"
}

ADMIN_TOKEN="$(resolve_token)"

# Prints the response body. Fails the script on any non-2xx, so no caller has to
# remember to check: a silent 403 must not read as an empty fleet.
cp_api() {
  local method="$1" path="$2" body="${3:-}" response status
  local -a args=(--silent --show-error --location --max-time 60
                 --write-out $'\n%{http_code}' --request "$method"
                 "${CONTROL_PLANE_URL}${path}"
                 --header 'Accept: application/json')
  if [[ -n "$body" ]]; then
    args+=(--header 'Content-Type: application/json' --data "$body")
  fi
  response="$(printf 'header = "Authorization: Bearer %s"\n' "$ADMIN_TOKEN" \
    | curl "${args[@]}" --config - )" || fail "control plane request failed: $method $path"
  status="${response##*$'\n'}"
  body="${response%$'\n'*}"
  [[ "$status" =~ ^2 ]] || fail "control plane returned $status for $method $path: $body"
  printf '%s' "$body"
}

fleet_json() {
  cp_api GET /tenants
}

# slug<TAB>status<TAB>runtimeHealth<TAB>deployedSha, one line per tenant.
fleet_rows() {
  fleet_json | python3 -c '
import json, sys
doc = json.load(sys.stdin)
for item in doc.get("items", []):
    runtime = item.get("runtime") or {}
    print("\t".join([
        item.get("slug", "?"),
        item.get("status", "?"),
        item.get("runtimeHealth", "?"),
        runtime.get("deployedSha") or "-",
    ]))
'
}

is_external() {
  local slug="$1" candidate
  for candidate in $EXTERNAL_SLUGS; do
    [[ "$slug" == "$candidate" ]] && return 0
  done
  return 1
}

print_report() {
  printf '\n==> Fleet at %s\n' "$TARGET"
  printf '%-24s %-10s %-12s %-12s %s\n' SLUG STATUS RUNTIME REVISION MATCHES_TARGET
  local slug status health sha matches
  while IFS=$'\t' read -r slug status health sha; do
    [[ -n "$slug" ]] || continue
    if [[ "$sha" == "$TARGET" || "$sha" == "sha-$TARGET" ]]; then
      matches=yes
    else
      matches=no
    fi
    printf '%-24s %-10s %-12s %-12s %s\n' \
      "$slug" "$status" "$health" "${sha:0:12}" "$matches"
  done < <(fleet_rows)
}

# Ordered list of provisioner-managed tenants: canaries first, then the rest
# alphabetically. Externally-owned tenants are excluded — the shared blue-green
# already deployed them, and a provisioning job for one would try to stand up a
# second stack for a tenant that does not have one.
provisioned_slugs() {
  local slug status health sha candidate
  local -a canaries=() others=()
  while IFS=$'\t' read -r slug status health sha; do
    [[ -n "$slug" ]] || continue
    [[ "$status" == "active" ]] || continue
    is_external "$slug" && continue
    if [[ -n "$ONLY_SLUG" && "$slug" != "$ONLY_SLUG" ]]; then
      continue
    fi
    local is_canary=0
    for candidate in $CANARY_SLUGS; do
      [[ "$slug" == "$candidate" ]] && is_canary=1
    done
    if (( is_canary )); then
      canaries+=("$slug")
    else
      others+=("$slug")
    fi
  done < <(fleet_rows)
  printf '%s\n' "${canaries[@]}" "${others[@]}" 2>/dev/null | awk 'NF'
}

tenant_health() {
  local slug="$1" row_slug status health sha
  while IFS=$'\t' read -r row_slug status health sha; do
    [[ "$row_slug" == "$slug" ]] || continue
    printf '%s' "$health"
    return 0
  done < <(fleet_rows)
  printf 'unknown'
}

# Polls the tenant's provisioning jobs until the newest one reaches a terminal
# phase. `phase` is the Control Plane's own mapping (model::provisioning_phase),
# so this does not re-derive which raw statuses are terminal.
poll_tenant_job() {
  local slug="$1" deadline phase last_notice=0
  deadline=$((SECONDS + JOB_TIMEOUT))
  while (( SECONDS < deadline )); do
    phase="$(cp_api GET "/tenants/${slug}/provisioning" | python3 -c '
import json, sys
doc = json.load(sys.stdin)
items = doc if isinstance(doc, list) else doc.get("items", [])
print(items[0].get("phase", "unknown") if items else "missing")
')"
    case "$phase" in
      completed)
        printf 'TENANT_DEPLOY=PASS slug=%s\n' "$slug"
        return 0
        ;;
      failed)
        cp_api GET "/tenants/${slug}/provisioning" | python3 -c '
import json, sys
doc = json.load(sys.stdin)
items = doc if isinstance(doc, list) else doc.get("items", [])
job = items[0] if items else {}
print("  status=%s error=%s detail=%s" % (
    job.get("status"), job.get("errorCode"), job.get("errorDetail")))
' >&2
        return 1
        ;;
    esac
    if (( SECONDS - last_notice >= 30 )); then
      printf '... %s is %s\n' "$slug" "$phase"
      last_notice=$SECONDS
    fi
    sleep "$JOB_POLL"
  done
  printf 'TENANT_DEPLOY=TIMEOUT slug=%s after=%ss\n' "$slug" "$JOB_TIMEOUT" >&2
  return 1
}

deploy_one_tenant() {
  local slug="$1" health
  health="$(tenant_health "$slug")"
  if [[ "$health" != "healthy" ]] && (( ! ALLOW_UNHEALTHY )); then
    fail "tenant $slug runtime is '$health', not healthy; fix it or pass --allow-unhealthy"
  fi
  printf '\n==> %s: requesting sha-%s (runtime was %s)\n' "$slug" "$TARGET" "$health"
  cp_api POST "/tenants/${slug}/provisioning/deploy" \
    "$(printf '{"desiredVersion":"sha-%s"}' "$TARGET")" >/dev/null
  poll_tenant_job "$slug"
}

if (( REPORT_ONLY )); then
  print_report
  exit 0
fi

# --- Phase 1: the shared stack, which is how externally-owned tenants ship ----
if (( SKIP_SHARED )); then
  printf '==> Skipping the shared stack (--skip-shared)\n'
else
  printf '==> Shared stack (%s) at %s\n' "$EXTERNAL_SLUGS" "$TARGET"
  CROWDRELAY_DEPLOY_IMAGE_SOURCE=local bash "$ROOT_DIR/scripts/deploy.sh" "$TARGET" \
    || fail 'shared stack deploy failed; no tenant was rolled'
  printf 'SHARED_DEPLOY=PASS sha=%s\n' "$TARGET"
fi

# --- Phase 2: provisioner-managed tenants, canary first, halt on failure ------
mapfile -t slugs < <(provisioned_slugs)

if (( ${#slugs[@]} == 0 )); then
  printf '\n==> No provisioner-managed tenants to roll\n'
else
  printf '\n==> Rolling %d tenant(s): %s\n' "${#slugs[@]}" "${slugs[*]}"
  rolled=0
  for slug in "${slugs[@]}"; do
    if ! deploy_one_tenant "$slug"; then
      printf '\nFLEET_ROLLOUT=HALTED slug=%s rolled=%d remaining=%d\n' \
        "$slug" "$rolled" "$(( ${#slugs[@]} - rolled - 1 ))" >&2
      printf 'Tenants already rolled stay on %s. They are NOT rolled back:\n' "$TARGET" >&2
      printf 'a partial fleet on a good revision beats an automatic mass revert.\n' >&2
      print_report
      exit 1
    fi
    rolled=$(( rolled + 1 ))
  done
  printf '\nFLEET_ROLLOUT=PASS sha=%s tenants=%d\n' "$TARGET" "$rolled"
fi

print_report
