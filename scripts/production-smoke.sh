#!/usr/bin/env bash
set -Eeuo pipefail

: "${CROWDRELAY_BASE_URL:?CROWDRELAY_BASE_URL is required}"
: "${VIRYA_BASE_URL:?VIRYA_BASE_URL is required}"
SYNESTHESIA_BASE_URL="${SYNESTHESIA_BASE_URL:-https://synesthesia.virya.music}"
SMOKE_STATE_DIR="${SMOKE_STATE_DIR:-${TMPDIR:-/tmp}/virya-production-smoke}"
ALERT_COOLDOWN_SECONDS="${ALERT_COOLDOWN_SECONDS:-3600}"
mkdir -p "$SMOKE_STATE_DIR"

meta_file=""
cleanup() {
  [[ -z "$meta_file" ]] || rm -f "$meta_file"
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

require_200 crowdrelay_live "${CROWDRELAY_BASE_URL%/}/v1/health/live"
require_200 crowdrelay_ready "${CROWDRELAY_BASE_URL%/}/v1/health/ready"
require_200 crowdrelay_metrics "${CROWDRELAY_BASE_URL%/}/metrics"
meta_file="$(mktemp)"
curl --fail-with-body --silent --show-error --location --connect-timeout 4 --max-time 10 \
  --retry 1 --retry-delay 1 --retry-all-errors --output "$meta_file" "${CROWDRELAY_BASE_URL%/}/v1/meta"
jq -e '.apiVersion == "1" and (.schemaVersion >= 40) and (.minimumPostgresServerVersionNum >= 180000) and .capabilities.area_wallet_postgres_v2 and .capabilities.area_vouchers_v2 and .capabilities.area_ticket_rewards_v2 and .capabilities.signal_fan_context_v1 and .capabilities.synesthesia_rewards_v1 and .capabilities.ticketing_v1' "$meta_file" >/dev/null
printf 'crowdrelay_meta_contract=ok\n'
require_200 crowdrelay_area_catalog "${CROWDRELAY_BASE_URL%/}/v1/public/area/drops"
require_200 crowdrelay_events "${CROWDRELAY_BASE_URL%/}/v1/public/events"
require_200 virya_home "${VIRYA_BASE_URL%/}/"
require_200 synesthesia_home "${SYNESTHESIA_BASE_URL%/}/"
require_200 synesthesia_boot_art "${SYNESTHESIA_BASE_URL%/}/menu-eye-poster.webp"
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
