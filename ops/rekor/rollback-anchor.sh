#!/usr/bin/env bash
set -Eeuo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"
compose_file="${CROWDRELAY_COMPOSE_FILE:-compose.oracle.yaml}"
secret_dir="${CROWDRELAY_SECRET_DIR:-deploy/secrets}"
CROWDRELAY_ADMIN_API_KEY_FILE="$secret_dir/crowdrelay_admin_api_key" python3 scripts/rekor-disable.py
docker compose -f "$compose_file" stop rekor-proof-anchor
printf 'External anchoring disabled and Rekor anchor stopped. Local proofs remain available.\n'
