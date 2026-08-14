#!/usr/bin/env bash
set -Eeuo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"
compose_file="${CROWDRELAY_COMPOSE_FILE:-compose.oracle.yaml}"
env_file="${CROWDRELAY_REKOR_ENV_FILE:-deploy/rekor-anchor.env}"
secret_dir="${CROWDRELAY_SECRET_DIR:-deploy/secrets}"

fail() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
[[ "${CROWDRELAY_IMAGE_TAG:-}" =~ ^sha-[0-9a-f]{40,64}$ ]] || fail 'CROWDRELAY_IMAGE_TAG must be an exact validated sha-<40..64 lowercase hex> tag'
[[ -f "$env_file" ]] || fail "$env_file is missing; copy deploy/rekor-anchor.env.example first"
grep -Eq '^CROWDRELAY_INTERNAL_URL=http://(crowdrelay-api|api):8080/?$' "$env_file" \
  || fail 'CROWDRELAY_INTERNAL_URL must use the private Docker API endpoint, not public Caddy/HTTPS'
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

expected_git_sha="${CROWDRELAY_IMAGE_TAG#sha-}"
[[ "$expected_git_sha" =~ ^[0-9a-f]{40}$ ]] || fail 'Rekor canary requires an exact 40-char source SHA'

# The relayer has no public port. Verify readiness and the durable confirmation
# journal from inside the isolated container before the production canary.
docker exec "$container" node -e '
const fs=require("node:fs");
fetch("http://127.0.0.1:8081/health/ready").then(async r=>{
  const body=await r.text();
  if(!r.ok) throw new Error(`relayer readiness ${r.status}: ${body.slice(0,300)}`);
  const p="/data/pending-confirmation.json";
  if(fs.existsSync(p) && fs.statSync(p).size>0) throw new Error("pending confirmation journal is not empty before canary");
}).catch(e=>{console.error(e.message);process.exit(1)})' \
  || fail 'Rekor relayer pre-canary readiness/journal check failed'

CROWDRELAY_ADMIN_API_KEY_FILE="$secret_dir/crowdrelay_admin_api_key" \
CROWDRELAY_EXPECTED_GIT_SHA="$expected_git_sha" \
  python3 scripts/rekor-canary.py

# A successful canary must leave the relayer ready and without an unconfirmed
# durable receipt. This catches confirm-contract drift even when publish itself succeeded.
docker exec "$container" node -e '
const fs=require("node:fs");
fetch("http://127.0.0.1:8081/health/ready").then(async r=>{
  const body=await r.text();
  if(!r.ok) throw new Error(`relayer readiness ${r.status}: ${body.slice(0,300)}`);
  const p="/data/pending-confirmation.json";
  if(fs.existsSync(p) && fs.statSync(p).size>0) throw new Error("pending confirmation journal remains after confirmed canary");
}).catch(e=>{console.error(e.message);process.exit(1)})' \
  || fail 'Rekor relayer post-canary readiness/journal check failed'

printf 'Rekor anchor is healthy and the public canary was confirmed.\n'
