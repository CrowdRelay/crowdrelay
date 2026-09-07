#!/usr/bin/env bash
set -Eeuo pipefail

: "${CROWDRELAY_BASE_URL:?CROWDRELAY_BASE_URL is required}"
: "${VIRYA_BASE_URL:?VIRYA_BASE_URL is required}"
SYNESTHESIA_BASE_URL="${SYNESTHESIA_BASE_URL:-https://synesthesia.virya.music}"
SMOKE_STATE_DIR="${SMOKE_STATE_DIR:-${TMPDIR:-/tmp}/crowdrelay-production-smoke}"
ALERT_COOLDOWN_SECONDS="${ALERT_COOLDOWN_SECONDS:-3600}"
mkdir -p "$SMOKE_STATE_DIR"

meta_file=""
tenant_file=""
metrics_file=""
cleanup() {
  [[ -z "$meta_file" ]] || rm -f "$meta_file"
  [[ -z "$tenant_file" ]] || rm -f "$tenant_file"
  [[ -z "$metrics_file" ]] || rm -f "$metrics_file"
}
trap cleanup EXIT

post_alert() {
  local message="$1"
  [[ -n "${ALERT_WEBHOOK_URL:-}" ]] || return 0
  local payload
  payload="$(jq -nc --arg content "$message" '{content:$content}')"
  curl --fail --silent --show-error --connect-timeout 4 --max-time 10 \
    --header 'content-type: application/json' --data "$payload" "$ALERT_WEBHOOK_URL" >/dev/null || true
}

on_error() {
  local rc="$1" line="$2" command="$3" now last=0
  trap - ERR
  now="$(date +%s)"
  [[ ! -f "$SMOKE_STATE_DIR/last-alert-at" ]] || read -r last < "$SMOKE_STATE_DIR/last-alert-at" || last=0
  : > "$SMOKE_STATE_DIR/failed"
  if (( now - last >= ALERT_COOLDOWN_SECONDS )); then
    post_alert "🚨 Virya production smoke failed on $(hostname) (line ${line}, exit ${rc}): ${command:0:900}"
    printf '%s\n' "$now" > "$SMOKE_STATE_DIR/last-alert-at"
  fi
  exit "$rc"
}
trap 'on_error "$?" "$LINENO" "$BASH_COMMAND"' ERR

request_status() {
  curl --silent --show-error --location --connect-timeout 4 --max-time 10 \
    --retry 1 --retry-delay 1 --retry-all-errors --output /dev/null --write-out '%{http_code}' "$@"
}
require_200() {
  local label="$1" url="$2" status
  status="$(request_status "$url")"
  [[ "$status" == 200 ]] || { printf '%s returned HTTP %s\n' "$label" "$status" >&2; return 1; }
  printf '%s=ok\n' "$label"
}

probe_edge_timing() {
  local label="$1" url="$2" headers timing server_timing
  headers="$(mktemp)"
  timing="$(curl --fail-with-body --silent --show-error --location --connect-timeout 4 --max-time 10 \
    --retry 1 --retry-delay 1 --retry-all-errors --dump-header "$headers" --output /dev/null \
    --write-out 'connect_s=%{time_connect} ttfb_s=%{time_starttransfer} total_s=%{time_total}' "$url")"
  server_timing="$(tr -d '\r' < "$headers" | awk -F': ' 'tolower($1)=="server-timing" {print $2; exit}')"
  rm -f "$headers"
  printf 'edge_timing label=%s %s server_timing=%s\n' "$label" "$timing" "${server_timing:-missing}"
}

require_200 crowdrelay_live "${CROWDRELAY_BASE_URL%/}/v1/health/live"
require_200 crowdrelay_ready "${CROWDRELAY_BASE_URL%/}/v1/health/ready"
metrics_file="$(mktemp)"
curl --fail-with-body --silent --show-error --location --connect-timeout 4 --max-time 10 \
  --retry 1 --retry-delay 1 --retry-all-errors --output "$metrics_file" "${CROWDRELAY_BASE_URL%/}/metrics"
grep -Fxq 'crowdrelay_ops_metrics_snapshot_available 1' "$metrics_file"
printf 'crowdrelay_metrics=ok ops_snapshot=available\n'
tenant_file="$(mktemp)"
curl --fail-with-body --silent --show-error --location --connect-timeout 4 --max-time 10 \
  --retry 1 --retry-delay 1 --retry-all-errors --output "$tenant_file" "${CROWDRELAY_BASE_URL%/}/v1/public/tenant/config"
jq -e '.regional.timezone | type == "string" and length > 0' "$tenant_file" >/dev/null
jq -e '.regional.currency | type == "string" and length == 3' "$tenant_file" >/dev/null
printf 'crowdrelay_tenant_config=ok\n'
meta_file="$(mktemp)"
curl --fail-with-body --silent --show-error --location --connect-timeout 4 --max-time 10 \
  --retry 1 --retry-delay 1 --retry-all-errors --output "$meta_file" "${CROWDRELAY_BASE_URL%/}/v1/meta"
# One jq expression covering fourteen assertions fails as one opaque line:
# the alert quotes the whole expression and names none of the fields, so a
# missing buildTimestamp and a missing capability look identical and cost the
# same twenty minutes to tell apart. Same assertions, reported individually.
meta_failures="$(jq -r '
  . as $meta
  | [
    (if .apiVersion == "1" then empty else "apiVersion=\(.apiVersion // "null")" end),
    (if (.schemaVersion // 0) >= 45 then empty else "schemaVersion=\(.schemaVersion // "null")" end),
    (if (.gitSha | type == "string" and test("^[0-9a-f]{40}$")) then empty else "gitSha=\(.gitSha // "null")" end),
    (if (.buildTimestamp | type == "string" and length > 0) then empty else "buildTimestamp=\(.buildTimestamp // "null") (image not built by CI?)" end),
    (if (.minimumPostgresServerVersionNum // 0) >= 180000 then empty else "minimumPostgresServerVersionNum=\(.minimumPostgresServerVersionNum // "null")" end)
  ]
  + ([
      "area_wallet_postgres_v2","area_vouchers_v2","area_ticket_rewards_v2",
      "signal_fan_context_v1","synesthesia_rewards_v1","synesthesia_leaderboard_v1",
      "ticketing_v1","communication_delivery_ledger_v1","tenant_regional_profile_v1"
    ] | map(. as $c | if ($meta.capabilities[$c] // false) then empty else "capability:\($c)" end))
  | join(", ")' "$meta_file")"
if [[ -n "$meta_failures" ]]; then
  printf 'crowdrelay meta contract failed: %s\n' "$meta_failures" >&2
  exit 1
fi
printf 'crowdrelay_meta_contract=ok\n'
require_200 crowdrelay_area_catalog "${CROWDRELAY_BASE_URL%/}/v1/public/area/drops"
require_200 crowdrelay_events "${CROWDRELAY_BASE_URL%/}/v1/public/events"
probe_edge_timing crowdrelay_ready "${CROWDRELAY_BASE_URL%/}/v1/health/ready"
probe_edge_timing crowdrelay_events "${CROWDRELAY_BASE_URL%/}/v1/public/events"

cors_headers="$(mktemp)"
curl --fail-with-body --silent --show-error --location --connect-timeout 4 --max-time 10 \
  --header "Origin: ${SYNESTHESIA_BASE_URL%/}" --dump-header "$cors_headers" --output /dev/null \
  "${CROWDRELAY_BASE_URL%/}/v1/public/events"
if ! tr -d '\r' < "$cors_headers" | grep -Fqi "access-control-allow-origin: ${SYNESTHESIA_BASE_URL%/}"; then
  printf 'crowdrelay CORS does not allow Synesthesia origin %s\n' "${SYNESTHESIA_BASE_URL%/}" >&2
  rm -f "$cors_headers"
  exit 1
fi
rm -f "$cors_headers"
printf 'crowdrelay_synesthesia_cors=ok\n'

require_200 virya_home "${VIRYA_BASE_URL%/}/"
require_200 synesthesia_home "${SYNESTHESIA_BASE_URL%/}/"
require_200 synesthesia_boot_art "${SYNESTHESIA_BASE_URL%/}/menu-world.webp"
if [[ -n "${N8N_INGRESS_URL:-}" ]]; then
  status="$(request_status --request POST --header 'content-type: application/json' --data '{"external_smoke":true}' "$N8N_INGRESS_URL")"
  [[ "$status" == 400 || "$status" == 401 ]] || { printf 'n8n signed ingress returned HTTP %s, expected 400 or 401\n' "$status" >&2; exit 1; }
  printf 'n8n_signed_ingress=ok_rejected_unsigned\n'
fi

if [[ -f "$SMOKE_STATE_DIR/failed" ]]; then
  post_alert "✅ Virya production smoke recovered on $(hostname)."
fi
rm -f "$SMOKE_STATE_DIR/failed" "$SMOKE_STATE_DIR/last-alert-at"
printf 'production_smoke=ok\n'
