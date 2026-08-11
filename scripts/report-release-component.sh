#!/usr/bin/env bash
set -Eeuo pipefail
: "${CROWDRELAY_BASE_URL:?}"
: "${CROWDRELAY_COMMERCE_API_KEY:?}"
: "${COMPONENT_KEY:?}"
SOURCE_SHA="${SOURCE_SHA:-${GITHUB_SHA:-unknown}}"
OBSERVED_AT="${OBSERVED_AT:-$(date -u +%Y-%m-%dT%H:%M:%SZ)}"
jq -n --arg component "$COMPONENT_KEY" --arg sha "$SOURCE_SHA" --arg digest "${ARTIFACT_DIGEST:-}" --arg deploy "${DEPLOY_REF:-}" --arg version "${COMPONENT_VERSION:-}" --arg manifest "${MANIFEST_SHA:-}" --arg observed "$OBSERVED_AT" '{component_key:$component,environment:"production",source_sha:$sha,artifact_digest:(if $digest=="" then null else $digest end),deploy_ref:(if $deploy=="" then null else $deploy end),version:(if $version=="" then null else $version end),manifest_sha:(if $manifest=="" then null else $manifest end),metadata:{reporter:"deploy"},observed_at:$observed}' | curl --fail-with-body --silent --show-error --max-time 15 -H 'content-type: application/json' -H "Authorization: Bearer ${CROWDRELAY_COMMERCE_API_KEY}" --data-binary @- "${CROWDRELAY_BASE_URL%/}/v1/internal/autopilot/release-components"
