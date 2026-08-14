#!/usr/bin/env bash
set -Eeuo pipefail
: "${CROWDRELAY_BASE_URL:?}"
: "${CROWDRELAY_COMMERCE_API_KEY:?}"
: "${N8N_EXECUTOR_ID:?}"
: "${N8N_EXECUTOR_VERSION:?}"
: "${N8N_WORKFLOW_ATTESTATION:?path to final secretless attestation}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
payload="$(mktemp "${TMPDIR:-/tmp}/viryaos-heartbeat.XXXXXX.json")"
trap 'rm -f "$payload"' EXIT
python3 "$ROOT/scripts/build_n8n_executor_heartbeat.py" \
  --manifest "$ROOT/n8n/viryaos-production-workflow-manifest.tsv" \
  --attestation "$N8N_WORKFLOW_ATTESTATION" \
  --executor-id "$N8N_EXECUTOR_ID" \
  --version "$N8N_EXECUTOR_VERSION" \
  --ttl-minutes "${N8N_HEARTBEAT_TTL_MINUTES:-90}" \
  --output "$payload"
curl --fail-with-body --silent --show-error --max-time 15 \
  -H 'content-type: application/json' \
  -H "Authorization: Bearer ${CROWDRELAY_COMMERCE_API_KEY}" \
  --data-binary "@$payload" \
  "${CROWDRELAY_BASE_URL%/}/v1/internal/autopilot/executors/heartbeat"
printf '\nN8N_HEARTBEAT_PUBLISH=PASS executor=%s\n' "$N8N_EXECUTOR_ID"
