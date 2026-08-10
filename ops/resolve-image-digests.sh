#!/usr/bin/env bash
set -Eeuo pipefail

# Resolve one validated full-commit sha-* release to immutable GHCR digests.
# This does not deploy anything. Source the generated env and use the digest
# compose override for the actual compose operation.

TAG="${CROWDRELAY_IMAGE_TAG:-}"
[[ "$TAG" =~ ^sha-[0-9a-f]{40}$ ]] || { echo 'CROWDRELAY_IMAGE_TAG must be sha-<full 40-char git SHA>' >&2; exit 2; }
API_REPO="${CROWDRELAY_API_IMAGE:-ghcr.io/wojciechbator/crowdrelay-api}"
WORKER_REPO="${CROWDRELAY_WORKER_IMAGE:-ghcr.io/wojciechbator/crowdrelay-worker}"
OUT="${DIGEST_ENV_FILE:-deploy/.image-digests.env}"

resolve() {
  local repo="$1" ref="${1}:${TAG}" digest
  docker pull "$ref" >/dev/null
  digest="$(docker image inspect --format '{{range .RepoDigests}}{{println .}}{{end}}' "$ref" | awk -v p="$repo@sha256:" 'index($0,p)==1 {print; exit}')"
  [[ "$digest" =~ ^${repo//./\\.}@sha256:[0-9a-f]{64}$ ]] || { echo "Could not resolve immutable digest for $ref" >&2; exit 1; }
  printf '%s' "$digest"
}

api_ref="$(resolve "$API_REPO")"
worker_ref="$(resolve "$WORKER_REPO")"
mkdir -p "$(dirname "$OUT")"
umask 077
cat > "$OUT" <<ENV
CROWDRELAY_API_IMAGE_REF=$api_ref
CROWDRELAY_WORKER_IMAGE_REF=$worker_ref
ENV
sha256sum "$OUT" > "$OUT.sha256"
echo "IMAGE_DIGEST_RESOLUTION=PASS tag=$TAG output=$OUT"
echo "Deploy with: docker compose --env-file $OUT -f compose.production.yaml -f compose.production.digest.yaml up -d"
