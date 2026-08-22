#!/usr/bin/env bash
# Publishes one runtime status receipt to the Control Plane.
#
# The Control Plane classifies a tenant as unknown until something reports this
# endpoint, and as stale once the last receipt ages past its freshness window.
# Deploy reports once so the panel is correct immediately after a release;
# `crowdrelayctl heartbeat` reports on a schedule so it stays that way.
#
# The caller collects the values; this script only shapes and sends them. That
# keeps the credential handling in one place and lets both callers reuse it.
set -Eeuo pipefail
: "${CONTROL_PLANE_BASE_URL:?}"
: "${CONTROL_PLANE_TELEMETRY_TOKEN:?}"
: "${CONTROL_PLANE_TENANT_SLUG:?}"

API_HEALTHY="${API_HEALTHY:-}"
WORKER_HEALTHY="${WORKER_HEALTHY:-}"
DEPLOYED_SHA="${DEPLOYED_SHA:-}"
OUTBOX_PENDING="${OUTBOX_PENDING:-}"
QUEUE_LAG="${QUEUE_LAG:-}"
SCHEMA_VERSION="${SCHEMA_VERSION:-}"
OBSERVED_AT="${OBSERVED_AT:-$(date -u +%Y-%m-%dT%H:%M:%SZ)}"

# The request rejects unknown fields, so every optional value is either a typed
# JSON value or null — never an empty string.
jq -n \
  --arg api "$API_HEALTHY" \
  --arg worker "$WORKER_HEALTHY" \
  --arg sha "$DEPLOYED_SHA" \
  --arg outbox "$OUTBOX_PENDING" \
  --arg lag "$QUEUE_LAG" \
  --arg schema "$SCHEMA_VERSION" \
  --arg observed "$OBSERVED_AT" \
  '{
    apiHealthy:(if $api=="" then null else ($api=="true") end),
    workerHealthy:(if $worker=="" then null else ($worker=="true") end),
    schemaVersion:(if $schema=="" then null else ($schema|tonumber) end),
    deployedSha:(if $sha=="" then null else $sha end),
    outboxPending:(if $outbox=="" then null else ($outbox|tonumber) end),
    queueLag:(if $lag=="" then null else ($lag|tonumber) end),
    lastHeartbeatAt:$observed
  }' | curl --fail-with-body --silent --show-error --max-time 15 \
    --request PUT \
    -H 'content-type: application/json' \
    -H "Authorization: Bearer ${CONTROL_PLANE_TELEMETRY_TOKEN}" \
    --data-binary @- \
    "${CONTROL_PLANE_BASE_URL%/}/api/v1/tenants/${CONTROL_PLANE_TENANT_SLUG}/runtime" >/dev/null
