#!/usr/bin/env bash
# Report this deployment's runtime status to the Control Plane.
#
# Invoked by `crowdrelayctl heartbeat` (the crowdrelay-heartbeat.timer systemd
# unit, every 60s) and at the tail of a verified deploy. crowdrelayctl probes
# health itself and passes the observations in as environment variables; this
# script only sanitizes them against the Control Plane's runtime contract and
# PUTs them.
#
# Fail-open by contract: crowdrelayctl treats a non-zero exit as a warning
# ("production deploy remains valid"), never as a deploy failure.
set -Eeuo pipefail

: "${CONTROL_PLANE_BASE_URL:?missing CONTROL_PLANE_BASE_URL}"
: "${CONTROL_PLANE_TELEMETRY_TOKEN:?missing CONTROL_PLANE_TELEMETRY_TOKEN}"
: "${CONTROL_PLANE_TENANT_SLUG:?missing CONTROL_PLANE_TENANT_SLUG}"

# The slug becomes a URL path segment; the Control Plane validates it as
# 2-63 lowercase letters/digits with internal hyphens. Reject anything else
# here so a malformed local config cannot smuggle characters into the path.
slug="$(printf '%s' "$CONTROL_PLANE_TENANT_SLUG" | tr '[:upper:]' '[:lower:]')"
if [[ ! "$slug" =~ ^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$ ]]; then
  echo "CONTROL_PLANE_TENANT_SLUG is not a valid tenant slug: ${CONTROL_PLANE_TENANT_SLUG}" >&2
  exit 1
fi

# Empty means "unobserved" and is sent as null — a probe failure must not
# overwrite the last known good state with a fabricated false. Only an
# explicit "true"/"false" becomes a boolean.
json_bool() {
  case "${1:-}" in
    true) printf 'true' ;;
    false) printf 'false' ;;
    *) printf 'null' ;;
  esac
}

# The Control Plane accepts only a 7-128 character hex identifier. A
# non-conforming local SHA (e.g. a tag name) degrades to null instead of
# failing validation and dropping the whole report.
deployed_sha_json="null"
if [[ "${DEPLOYED_SHA:-}" =~ ^[0-9a-fA-F]{7,128}$ ]]; then
  deployed_sha_json="\"${DEPLOYED_SHA}\""
fi

# Runtime counters are non-negative integers; anything else becomes null.
json_counter() {
  if [[ "${1:-}" =~ ^[0-9]+$ ]]; then
    printf '%s' "$1"
  else
    printf 'null'
  fi
}

# lastHeartbeatAt is the ordering key on the server: reports older than the
# stored heartbeat are ignored, and a report with no timestamp cannot refresh
# staleness — so it is always stamped here, never omitted.
observed_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

payload="$(printf \
  '{"apiHealthy":%s,"workerHealthy":%s,"schemaVersion":%s,"deployedSha":%s,"outboxPending":%s,"queueLag":%s,"awaitingApproval":%s,"lastHeartbeatAt":"%s"}' \
  "$(json_bool "${API_HEALTHY:-}")" \
  "$(json_bool "${WORKER_HEALTHY:-}")" \
  "$(json_counter "${SCHEMA_VERSION:-}")" \
  "$deployed_sha_json" \
  "$(json_counter "${OUTBOX_PENDING:-}")" \
  "$(json_counter "${QUEUE_LAG:-}")" \
  "$(json_counter "${AWAITING_APPROVAL:-}")" \
  "$observed_at")"

curl --fail-with-body --silent --show-error --max-time 15 \
  -X PUT \
  -H 'content-type: application/json' \
  -H "Authorization: Bearer ${CONTROL_PLANE_TELEMETRY_TOKEN}" \
  --data-binary "$payload" \
  "${CONTROL_PLANE_BASE_URL%/}/api/v1/tenants/${slug}/runtime"
