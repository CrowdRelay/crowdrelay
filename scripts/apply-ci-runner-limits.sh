#!/usr/bin/env bash
# Puts every self-hosted GitHub Actions runner on the production host into
# `ci-runners.slice` (deploy/systemd/ci-runners.slice), so CI builds yield the
# CPU and disk to production instead of competing with it.
#
# Run on the host, as a user with sudo:
#   bash scripts/apply-ci-runner-limits.sh --dry-run   # show what would change
#   bash scripts/apply-ci-runner-limits.sh             # apply
#
# Idempotent. A runner restarts only when its drop-in changed, and a job in
# flight is interrupted by that restart — run it between CI jobs.
set -Eeuo pipefail

DRY_RUN=false
[[ "${1:-}" == "--dry-run" ]] && DRY_RUN=true
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
SLICE_SRC="$ROOT_DIR/deploy/systemd/ci-runners.slice"
DROPIN_SRC="$ROOT_DIR/deploy/systemd/actions-runner-ci-slice.conf"
[[ -f "$SLICE_SRC" && -f "$DROPIN_SRC" ]] || { echo "missing unit sources under deploy/systemd" >&2; exit 2; }

run() {
  if [[ "$DRY_RUN" == true ]]; then printf 'would run: %s\n' "$*"; else "$@"; fi
}

mapfile -t runners < <(systemctl list-units --type=service --all --no-legend --plain 'actions.runner.*' | awk '{print $1}')
if [[ ${#runners[@]} -eq 0 ]]; then
  echo "no actions.runner.* services on this host — nothing to do"
  exit 0
fi

if ! cmp -s "$SLICE_SRC" /etc/systemd/system/ci-runners.slice 2>/dev/null; then
  run sudo install -m 0644 "$SLICE_SRC" /etc/systemd/system/ci-runners.slice
fi

changed=()
for unit in "${runners[@]}"; do
  dir="/etc/systemd/system/${unit}.d"
  if ! cmp -s "$DROPIN_SRC" "$dir/10-ci-slice.conf" 2>/dev/null; then
    run sudo install -d -m 0755 "$dir"
    run sudo install -m 0644 "$DROPIN_SRC" "$dir/10-ci-slice.conf"
    changed+=("$unit")
  fi
done

run sudo systemctl daemon-reload
for unit in "${changed[@]}"; do
  run sudo systemctl restart "$unit"
done

if [[ "$DRY_RUN" == false ]]; then
  for unit in "${runners[@]}"; do
    printf '%s slice=%s nice=%s\n' "$unit" \
      "$(systemctl show "$unit" -p Slice --value)" "$(systemctl show "$unit" -p Nice --value)"
  done
  systemctl show ci-runners.slice -p CPUWeight -p IOWeight -p MemoryHigh --no-pager
fi
echo "runners=${#runners[@]} changed=${#changed[@]} dry_run=$DRY_RUN"
