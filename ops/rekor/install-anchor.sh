#!/usr/bin/env bash
set -Eeuo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"
compose_file="${CROWDRELAY_COMPOSE_FILE:-compose.oracle.yaml}"
env_file="${CROWDRELAY_REKOR_ENV_FILE:-deploy/rekor-anchor.env}"
secret_dir="${CROWDRELAY_SECRET_DIR:-deploy/secrets}"

fail() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
[[ -n "${CROWDRELAY_IMAGE_TAG:-}" ]] || fail 'CROWDRELAY_IMAGE_TAG must be the exact validated sha-* tag'
[[ -f "$env_file" ]] || fail "$env_file is missing; copy deploy/rekor-anchor.env.example first"
for file in crowdrelay_commerce_api_key crowdrelay_admin_api_key rekor_signing_key.pem; do
  [[ -s "$secret_dir/$file" ]] || fail "$secret_dir/$file is missing or empty"
done
[[ "$(stat -c '%a' "$secret_dir/rekor_signing_key.pem")" =~ ^(400|600)$ ]] || fail 'Rekor private key must have mode 400 or 600'
openssl pkey -in "$secret_dir/rekor_signing_key.pem" -check -noout >/dev/null

docker compose -f "$compose_file" config --quiet
docker compose -f "$compose_file" pull rekor-proof-anchor
docker compose -f "$compose_file" up -d --no-deps rekor-proof-anchor

container="crowdrelay-rekor-proof-anchor"
deadline=$((SECONDS + 180))
while (( SECONDS < deadline )); do
  status="$(docker inspect --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' "$container" 2>/dev/null || true)"
  case "$status" in
    healthy) break ;;
    unhealthy|exited|dead)
      docker compose -f "$compose_file" logs --tail=120 rekor-proof-anchor >&2 || true
      fail "Rekor anchor entered $status"
      ;;
  esac
  sleep 3
done
[[ "${status:-}" == healthy ]] || {
  docker compose -f "$compose_file" logs --tail=120 rekor-proof-anchor >&2 || true
  fail 'Rekor anchor did not become healthy'
}

CROWDRELAY_ADMIN_API_KEY_FILE="$secret_dir/crowdrelay_admin_api_key" \
  python3 scripts/rekor-canary.py

printf 'Rekor anchor is healthy and the public canary was confirmed.\n'
