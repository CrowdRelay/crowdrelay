#!/usr/bin/env bash
set -Eeuo pipefail

# Idempotent production env var update + safe worker restart.
#
# Usage:
#   bash scripts/update-env.sh KEY=VALUE [KEY=VALUE ...]
#
# What it does:
#   1. Reads the production env file on the remote host.
#   2. For each KEY=VALUE: if KEY exists, replaces the line in place;
#      if not, appends it. Idempotent — safe to run multiple times.
#   3. Validates the file is non-empty afterwards.
#   4. Detects the active color (blue or green) from Caddyfile marker.
#   5. Recreates only the active worker with the correct compose files
#      and the exact running image tag. Never touches the API or DB.
#   6. Waits for worker health.
#   7. Verifies the new env vars are loaded.

REMOTE="${CROWDRELAY_DEPLOY_HOST:-virya-crowdrelay}"
REMOTE_REPO="${CROWDRELAY_DEPLOY_REMOTE_REPO:-/opt/crowdrelay}"
ENV_FILE="${REMOTE_REPO}/deploy/.env.production"
COMPOSE_FILE="${REMOTE_REPO}/compose.production.yaml"
BLUEGREEN_FILE="${REMOTE_REPO}/compose.bluegreen.yaml"
CADDYFILE="${REMOTE_REPO}/ops/edge/Caddyfile"

fail() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

[[ $# -ge 1 ]] || fail 'usage: update-env.sh KEY=VALUE [KEY=VALUE ...]'

# Validate arguments are KEY=VALUE pairs
for arg in "$@"; do
  [[ "$arg" == *=* ]] || fail "argument is not KEY=VALUE: $arg"
  key="${arg%%=*}"
  [[ "$key" =~ ^[A-Z][A-Z0-9_]*$ ]] || fail "key looks invalid: $key (must be uppercase snake_case)"
done

printf '==> 1/6 — Idempotent env update on %s\n' "$REMOTE"

# Build the remote script that updates the env file in place
REMOTE_SCRIPT=$(cat << 'INNER'
set -Eeuo pipefail
env_file="$1"
shift
tmpfile="$(mktemp)"
cp "$env_file" "$tmpfile"
for arg in "$@"; do
  key="${arg%%=*}"
  value="${arg#*=}"
  if grep -q "^${key}=" "$tmpfile"; then
    # Replace existing line in place — preserves inode via sed -i
    sed -i "s|^${key}=.*|${key}=${value}|" "$tmpfile"
    printf 'UPDATED %s\n' "$key"
  else
    printf '%s=%s\n' "$key" "$value" >> "$tmpfile"
    printf 'APPENDED %s\n' "$key"
  fi
done
# Validate non-empty
[[ -s "$tmpfile" ]] || { printf 'ERROR: env file is empty after update\n' >&2; rm -f "$tmpfile"; exit 1; }
# Atomic replace — cp preserves the inode
cat "$tmpfile" > "$env_file"
rm -f "$tmpfile"
printf 'ENV_FILE=OK lines=%s\n' "$(wc -l < "$env_file")"
INNER
)

ssh -T "$REMOTE" bash -s -- "$ENV_FILE" "$@" <<< "$REMOTE_SCRIPT"

printf '\n==> 2/6 — Detect active color\n'
ACTIVE_COLOR=$(ssh -T "$REMOTE" "grep '# CROWDRELAY_ACTIVE=' '$CADDYFILE'" 2>&1 | sed 's/.*CROWDRELAY_ACTIVE=//')
printf 'ACTIVE_COLOR=%s\n' "$ACTIVE_COLOR"
[[ "$ACTIVE_COLOR" == "blue" || "$ACTIVE_COLOR" == "green" ]] || fail "could not detect active color from Caddyfile"

if [[ "$ACTIVE_COLOR" == "blue" ]]; then
  WORKER_SERVICE="worker"
  WORKER_CONTAINER="crowdrelay-worker-1"
else
  WORKER_SERVICE="worker-green"
  WORKER_CONTAINER="crowdrelay-worker-green-1"
fi

printf '\n==> 3/6 — Get running image tag\n'
TAG=$(ssh -T "$REMOTE" "docker inspect '$WORKER_CONTAINER' --format '{{range .Config.Labels}}{{println .}}{{end}}'" 2>&1 | grep "^sha-" | head -1)
printf 'TAG=%s\n' "$TAG"
[[ "$TAG" == sha-* ]] || fail "could not detect running image tag"

printf '\n==> 4/6 — Recreate %s worker (no-deps, same image)\n' "$ACTIVE_COLOR"
ssh -T "$REMOTE" "cd '$REMOTE_REPO' && export CROWDRELAY_GREEN_TAG='$TAG' && docker compose --env-file '$ENV_FILE' -f '$COMPOSE_FILE' -f '$BLUEGREEN_FILE' up -d --no-deps --force-recreate '$WORKER_SERVICE'" 2>&1

printf '\n==> 5/6 — Wait for worker health\n'
for i in $(seq 1 30); do
  status=$(ssh -T "$REMOTE" "docker inspect '$WORKER_CONTAINER' --format '{{.State.Health.Status}}'" 2>&1)
  if [[ "$status" == "healthy" ]]; then
    printf 'WORKER_HEALTHY after %ss\n' "$((i * 2))"
    break
  fi
  [[ $i -eq 30 ]] && fail "worker did not become healthy in 60s (last status: $status)"
  sleep 2
done

printf '\n==> 6/6 — Verify env vars loaded\n'
for arg in "$@"; do
  key="${arg%%=*}"
  value=$(ssh -T "$REMOTE" "docker exec '$WORKER_CONTAINER' env" 2>&1 | grep "^${key}=" | head -1)
  printf '%s\n' "$value"
  [[ "$value" == "$arg" ]] || fail "env var mismatch: expected '$arg', got '$value'"
done

printf '\nUPDATE_ENV=PASS worker=%s vars=%d\n' "$WORKER_CONTAINER" "$#"
