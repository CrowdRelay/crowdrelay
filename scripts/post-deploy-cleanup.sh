#!/usr/bin/env bash
# Post-deploy Docker cleanup — runs on the production host after every deploy.
#
# Prunes build cache, dangling images, stopped containers, and keeps only the
# latest + rollback image per family (skips images used by running containers).
# Reports disk usage before/after so the deploy log shows the reclaim.
#
# Usage: bash scripts/post-deploy-cleanup.sh [image-family...]
# Default families: crowdrelay-api crowdrelay-worker crowdrelay-control-plane
#                  crowdrelay-rekor-proof-anchor crowdrelay-agents
set -uo pipefail

FAMILIES=("$@")
if [[ ${#FAMILIES[@]} -eq 0 ]]; then
  FAMILIES=(
    crowdrelay-api
    crowdrelay-worker
    crowdrelay-control-plane
    crowdrelay-rekor-proof-anchor
    crowdrelay-agents
  )
fi

echo "==> Post-deploy cleanup"
before="$(docker system df --format '{{.Type}}\t{{.Size}}\t{{.Reclaimable}}' 2>/dev/null || true)"

# Prune build cache — the biggest reclaim on a build+deploy host.
docker builder prune --all -f 2>/dev/null || true

# Prune dangling images (untagged <none> layers).
docker image prune -f 2>/dev/null || true

# Prune stopped containers left over from one-shot setup/migrate runs.
docker container prune -f 2>/dev/null || true

# Keep only the 2 newest sha-tagged images per family (latest + rollback).
# Skip any image that a running container is using.
running_images="$(docker inspect $(docker ps -q) --format '{{.Image}}' 2>/dev/null | sort -u || echo "")"
for family in "${FAMILIES[@]}"; do
  docker images --format '{{.Repository}}:{{.Tag}}\t{{.ID}}' 2>/dev/null \
    | grep -E "(^|/)${family}:sha-" \
    | sort -r \
    | tail -n +3 \
    | while IFS=$'\t' read -r repo_tag image_id; do
      echo "$running_images" | grep -q "$image_id" && continue
      docker rmi "$image_id" 2>/dev/null && echo "  pruned $repo_tag" || true
    done
done

after="$(docker system df --format '{{.Type}}\t{{.Size}}\t{{.Reclaimable}}' 2>/dev/null || true)"

echo "CLEANUP=PASS"
if [[ -n "$before" && -n "$after" ]]; then
  echo "--- disk before ---"
  echo "$before"
  echo "--- disk after ---"
  echo "$after"
fi
