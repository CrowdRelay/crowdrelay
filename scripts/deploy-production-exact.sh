#!/usr/bin/env bash
set -Eeuo pipefail
umask 077

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
TARGET="${1:-}"
REMOTE="${CROWDRELAY_DEPLOY_HOST:-virya-crowdrelay}"
REMOTE_REPO="${CROWDRELAY_DEPLOY_REMOTE_REPO:-/opt/crowdrelay}"
PUBLIC_BASE_URL="${CROWDRELAY_PUBLIC_BASE_URL:-https://signal-api.virya.music}"
IMAGE_ATTEMPTS="${CROWDRELAY_IMAGE_GATE_ATTEMPTS:-36}"
IMAGE_SLEEP_SECONDS="${CROWDRELAY_IMAGE_GATE_SLEEP_SECONDS:-10}"

fail() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

require() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

[[ "$TARGET" =~ ^[0-9a-f]{40}$ ]] || fail 'usage: deploy-production-exact.sh <full-40-character-lowercase-sha>'
[[ "$IMAGE_ATTEMPTS" =~ ^[1-9][0-9]*$ ]] || fail 'CROWDRELAY_IMAGE_GATE_ATTEMPTS must be a positive integer'
[[ "$IMAGE_SLEEP_SECONDS" =~ ^[1-9][0-9]*$ ]] || fail 'CROWDRELAY_IMAGE_GATE_SLEEP_SECONDS must be a positive integer'

for command in git ssh scp curl; do require "$command"; done

cd "$ROOT_DIR"
git cat-file -e "${TARGET}^{commit}" 2>/dev/null || fail "target commit is not present locally: $TARGET"

LOCAL_HEAD="$(git rev-parse HEAD)"
git merge-base --is-ancestor "$TARGET" "$LOCAL_HEAD" || \
  fail "target=$TARGET must be an ancestor of local HEAD=$LOCAL_HEAD"

printf '==> 1/5 — Local exact-SHA contract\n'
printf 'LOCAL_TARGET=PASS sha=%s local_head=%s target_is_ancestor=true\n' "$TARGET" "$LOCAL_HEAD"

printf '\n==> 2/5 — Production image gate\n'
ssh -T "$REMOTE" bash -s -- "$TARGET" "$IMAGE_ATTEMPTS" "$IMAGE_SLEEP_SECONDS" <<'REMOTE_IMAGE_GATE'
# Remote body runs as one brace group with stdin detached. bash reads this
# script from ssh stdin; any command that attaches stdin (docker compose
# run/exec) would otherwise swallow the remainder and silently skip it.
{
set -Eeuo pipefail
target="$1"
attempts="$2"
sleep_seconds="$3"

fail() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

command -v docker >/dev/null 2>&1 || fail 'docker is missing on production host'
command -v timeout >/dev/null 2>&1 || fail 'GNU timeout is missing on production host'

for component in api worker; do
  image="ghcr.io/crowdrelay/crowdrelay-${component}:sha-${target}"
  success=false

  # If the image is already present locally (e.g. built on-host or pre-loaded),
  # skip the registry pull. This supports airgapped hosts and private registries
  # where anonymous pull is not available.
  local_revision="$(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$image" 2>/dev/null || true)"
  if [[ "$local_revision" == "$target" ]]; then
    printf 'LOCAL image=%s revision=%s (skipping registry pull)\n' "$image" "$local_revision"
    success=true
  else
    for ((attempt=1; attempt<=attempts; attempt++)); do
      printf 'Checking %s (%d/%d)\n' "$image" "$attempt" "$attempts"
      output_file="$(mktemp)"
      if timeout 90s docker pull --quiet "$image" >"$output_file" 2>&1; then
        rm -f "$output_file"
        success=true
        break
      fi

      output="$(cat "$output_file")"
      rm -f "$output_file"

      case "$output" in
        *"no matching manifest"*)
          printf '%s\n' "$output" >&2
          fail "published image does not support the production platform: $image"
          ;;
        *"manifest unknown"*|*"not found"*|*"not published"*)
          if (( attempt == attempts )); then
            printf '%s\n' "$output" >&2
            fail "exact image did not become available: $image"
          fi
          printf 'WAIT image=%s reason=not-published-yet\n' "$image"
          sleep "$sleep_seconds"
          ;;
        *"context deadline exceeded"*|*"i/o timeout"*|*"TLS handshake timeout"*|*"temporary failure"*)
          if (( attempt == attempts )); then
            printf '%s\n' "$output" >&2
            fail "registry transport did not recover for $image"
          fi
          printf 'WAIT image=%s reason=registry-transport\n' "$image"
          sleep "$sleep_seconds"
          ;;
        *"unauthorized"*)
          # The registry may be private. If a local image with the correct
          # revision exists, use it instead of failing.
          if [[ -n "$local_revision" ]]; then
            printf 'WARN registry unauthorized for %s — using local image (revision=%s)\n' "$image" "$local_revision"
            success=true
            break
          fi
          if (( attempt == attempts )); then
            printf '%s\n' "$output" >&2
            fail "registry unauthorized and no local image for $image"
          fi
          printf 'WAIT image=%s reason=unauthorized\n' "$image"
          sleep "$sleep_seconds"
          ;;
        *)
          printf '%s\n' "$output" >&2
          fail "docker pull failed for $image"
          ;;
      esac
    done
  fi

  [[ "$success" == true ]] || fail "image gate exhausted unexpectedly: $image"

  revision="$(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$image" 2>/dev/null || true)"
  architecture="$(docker image inspect --format '{{.Architecture}}' "$image" 2>/dev/null || true)"
  # The release tag is a multi-arch manifest list, so the daemon already
  # resolved it to this host. The invariant is that the pulled image matches
  # the host that will run it, not one hardcoded architecture: amd64 on
  # virya-oracle, arm64 on virya-crowdrelay.
  host_architecture="$(docker version --format '{{.Server.Arch}}' 2>/dev/null || true)"
  [[ -n "$host_architecture" ]] || fail "could not resolve the production host architecture"

  [[ "$revision" == "$target" ]] || \
    fail "OCI revision mismatch for $image: got=$revision expected=$target"
  [[ "$architecture" == "$host_architecture" ]] || \
    fail "production image architecture mismatch for $image: got=$architecture expected=$host_architecture"

  printf 'EXACT_IMAGE=PASS component=%s revision=%s architecture=%s\n' \
    "$component" "$revision" "$architecture"
done

printf 'IMAGE_GATE=PASS sha=%s required=api,worker architecture=%s\n' "$target" "$host_architecture"
} </dev/null
REMOTE_IMAGE_GATE

printf '\n==> 3/5 — Exact source sync (local bundle, no GitHub auth on server)\n'
REMOTE_STATE="$(
  ssh -T "$REMOTE" bash -s -- "$REMOTE_REPO" <<'REMOTE_PREFLIGHT'
# Remote body runs as one brace group with stdin detached. bash reads this
# script from ssh stdin; any command that attaches stdin (docker compose
# run/exec) would otherwise swallow the remainder and silently skip it.
{
set -Eeuo pipefail
repo="$1"
[[ -d "$repo/.git" ]] || { echo "ERROR=repo-missing"; exit 20; }
cd "$repo"
head="$(git rev-parse HEAD)"
branch="$(git symbolic-ref --quiet --short HEAD 2>/dev/null || true)"
dirty="$(git status --porcelain --untracked-files=normal)"
[[ -n "$branch" ]] || { echo "ERROR=detached-head"; exit 21; }
[[ -z "$dirty" ]] || {
  echo "ERROR=worktree-dirty"
  printf '%s\n' "$dirty" >&2
  exit 22
}
# A directory copied in from another account leaves the deploy user unable to
# write inside it. Checkout would then fail halfway and leave the worktree
# dirty, which is far more work to recover than refusing here. An empty one
# holds nothing, so it is removed; anything else is reported untouched.
find . -path ./.git -prune -o -type d ! -writable -print 2>/dev/null |
  sort -r |
  while IFS= read -r directory; do
    rmdir "$directory" 2>/dev/null || true
  done
unwritable="$(find . -path ./.git -prune -o -type d ! -writable -print 2>/dev/null)"
[[ -z "$unwritable" ]] || {
  echo "ERROR=worktree-unwritable"
  printf '%s\n' "$unwritable" >&2
  exit 23
}
printf 'HEAD=%s\nBRANCH=%s\n' "$head" "$branch"
} </dev/null
REMOTE_PREFLIGHT
)" || fail 'production repository preflight failed'

REMOTE_HEAD="$(printf '%s\n' "$REMOTE_STATE" | sed -n 's/^HEAD=//p')"
REMOTE_BRANCH="$(printf '%s\n' "$REMOTE_STATE" | sed -n 's/^BRANCH=//p')"
[[ "$REMOTE_HEAD" =~ ^[0-9a-f]{40}$ ]] || fail "invalid remote HEAD from preflight: $REMOTE_STATE"
[[ -n "$REMOTE_BRANCH" ]] || fail "missing remote branch from preflight: $REMOTE_STATE"

git cat-file -e "${REMOTE_HEAD}^{commit}" 2>/dev/null || \
  fail "production HEAD=$REMOTE_HEAD is not present in the local git graph"
git merge-base --is-ancestor "$REMOTE_HEAD" "$TARGET" || \
  fail "refusing non-fast-forward production source move old=$REMOTE_HEAD target=$TARGET"

BUNDLE="$(mktemp -t crowdrelay-production.XXXXXX.bundle)"
REMOTE_BUNDLE="/tmp/crowdrelay-production-${TARGET}.bundle"
cleanup() {
  rm -f "$BUNDLE"
  ssh -T "$REMOTE" "rm -f '$REMOTE_BUNDLE'" >/dev/null 2>&1 || true
}
trap cleanup EXIT

git bundle create "$BUNDLE" HEAD >/dev/null
scp -q "$BUNDLE" "$REMOTE:$REMOTE_BUNDLE"

ssh -T "$REMOTE" bash -s -- \
  "$REMOTE_REPO" "$REMOTE_HEAD" "$REMOTE_BRANCH" "$TARGET" "$LOCAL_HEAD" "$REMOTE_BUNDLE" <<'REMOTE_SYNC'
# Remote body runs as one brace group with stdin detached. bash reads this
# script from ssh stdin; any command that attaches stdin (docker compose
# run/exec) would otherwise swallow the remainder and silently skip it.
{
set -Eeuo pipefail
repo="$1"
expected_old="$2"
expected_branch="$3"
target="$4"
source_head="$5"
bundle="$6"

fail() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

cd "$repo"
current="$(git rev-parse HEAD)"
branch="$(git symbolic-ref --quiet --short HEAD 2>/dev/null || true)"
[[ "$current" == "$expected_old" ]] || fail "production HEAD changed during deploy: got=$current expected=$expected_old"
[[ "$branch" == "$expected_branch" ]] || fail "production branch changed during deploy: got=$branch expected=$expected_branch"
[[ -z "$(git status --porcelain --untracked-files=normal)" ]] || fail 'production worktree became dirty during deploy'

bundle_head="$(git bundle list-heads "$bundle" | awk '$2 == "HEAD" {print $1}')"
[[ "$bundle_head" == "$source_head" ]] || fail "bundle HEAD mismatch: got=$bundle_head expected=$source_head"

git fetch --no-tags "$bundle" HEAD >/dev/null
fetched="$(git rev-parse FETCH_HEAD)"
[[ "$fetched" == "$source_head" ]] || fail "bundle fetch mismatch: got=$fetched expected=$source_head"

git cat-file -e "${target}^{commit}" 2>/dev/null || fail "target object was not imported from bundle"
git merge-base --is-ancestor "$current" "$target" || fail "target is not a fast-forward from production HEAD"
git merge-base --is-ancestor "$target" "$source_head" || fail "target is not an ancestor of bundle HEAD"

backup_ref="refs/backup/predeploy-$(date -u +%Y%m%dT%H%M%SZ)-${current:0:12}"
git update-ref "$backup_ref" "$current"
git merge --ff-only "$target" >/dev/null

final="$(git rev-parse HEAD)"
[[ "$final" == "$target" ]] || fail "source sync ended at $final, expected $target"
[[ -z "$(git status --porcelain --untracked-files=normal)" ]] || fail 'production worktree is dirty after source sync'

printf 'SOURCE_SYNC=PASS old=%s new=%s branch=%s backup=%s github-auth=not-required\n' \
  "$current" "$final" "$branch" "$backup_ref"
} </dev/null
REMOTE_SYNC

printf '\n==> 4/5 — Canonical crowdrelayctl deploy\n'
ssh -T "$REMOTE" bash -s -- "$REMOTE_REPO" "$TARGET" <<'REMOTE_DEPLOY'
# Remote body runs as one brace group with stdin detached. bash reads this
# script from ssh stdin; any command that attaches stdin (docker compose
# run/exec) would otherwise swallow the remainder and silently skip it.
{
set -Eeuo pipefail
repo="$1"
target="$2"
cd "$repo"

fail() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

[[ "$(git rev-parse HEAD)" == "$target" ]] || fail "repo HEAD drifted before canonical deploy"
[[ -z "$(git status --porcelain --untracked-files=normal)" ]] || fail 'repo worktree is dirty before canonical deploy'
[[ -x ./crowdrelayctl ]] || fail 'canonical crowdrelayctl is missing or not executable'

./crowdrelayctl pin "$target"
./crowdrelayctl doctor
./crowdrelayctl deploy

for service in api worker; do
  container_id="$(docker ps -q --filter "name=^crowdrelay-${service}-1$" | head -n1)"
  [[ -n "$container_id" ]] || fail "missing running container after deploy: $service"
  revision="$(docker inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$container_id" 2>/dev/null || true)"
  [[ "$revision" == "$target" ]] || fail "runtime OCI revision mismatch for $service: $revision"
done

printf 'CANONICAL_DEPLOY=PASS sha=%s engine=crowdrelayctl\n' "$target"
} </dev/null
REMOTE_DEPLOY

printf '\n==> 5/5 — Production git/runtime receipt + public health\n'
ssh -T "$REMOTE" bash -s -- "$REMOTE_REPO" "$TARGET" <<'REMOTE_RECEIPT'
# Remote body runs as one brace group with stdin detached. bash reads this
# script from ssh stdin; any command that attaches stdin (docker compose
# run/exec) would otherwise swallow the remainder and silently skip it.
{
set -Eeuo pipefail
repo="$1"
target="$2"
cd "$repo"

fail() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

head="$(git rev-parse HEAD)"
[[ "$head" == "$target" ]] || fail "production git HEAD mismatch: got=$head expected=$target"
[[ -z "$(git status --porcelain --untracked-files=normal)" ]] || fail 'production worktree is dirty at final receipt'

for service in api worker; do
  container_id="$(docker ps -q --filter "name=^crowdrelay-${service}-1$" | head -n1)"
  [[ -n "$container_id" ]] || fail "missing running container at final receipt: $service"
  revision="$(docker inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$container_id" 2>/dev/null || true)"
  [[ "$revision" == "$target" ]] || fail "runtime OCI revision mismatch for $service: got=$revision expected=$target"
done

printf 'PRODUCTION_EXACT_SHA=PASS source=git+oci sha=%s\n' "$target"
} </dev/null
REMOTE_RECEIPT

curl --fail --silent --show-error \
  --retry 5 --retry-delay 1 --retry-all-errors \
  --connect-timeout 3 --max-time 15 \
  "${PUBLIC_BASE_URL%/}/v1/health/ready" >/dev/null

printf 'PUBLIC_HEALTH=PASS url=%s\n' "${PUBLIC_BASE_URL%/}/v1/health/ready"

# Public metadata is useful diagnostics, but it is not release identity. A CDN,
# reverse-proxy or connection-drain window may briefly expose the previous
# metadata after the production git tree and runtime containers are already
# exact. Report that as telemetry instead of converting a successful deploy
# into a false negative.
public_meta="$(curl --silent --show-error --connect-timeout 3 --max-time 10 "${PUBLIC_BASE_URL%/}/v1/meta" 2>/dev/null || true)"
if [[ -n "$public_meta" ]]; then
  actual="$(printf '%s' "$public_meta" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("gitSha") or "")' 2>/dev/null || true)"
  if [[ "$actual" == "$TARGET" ]]; then
    printf 'PUBLIC_META=PASS gitSha=%s\n' "$actual"
  else
    printf 'PUBLIC_META=STALE observed=%s expected=%s blocking=false\n' "${actual:-unavailable}" "$TARGET" >&2
  fi
else
  printf 'PUBLIC_META=UNAVAILABLE blocking=false\n' >&2
fi

printf '\nDEPLOY=PASS sha-%s identity=git+oci public=health repo-sync=bundle github-auth-on-server=none\n' "$TARGET"
