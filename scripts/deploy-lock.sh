#!/usr/bin/env bash
# Deploy lock for FakApp coordination.
#
# Deploy scripts call `deploy-lock.sh acquire` before mutating production and
# `deploy-lock.sh release` after verification. FakApp checks the lock before
# remediating — if a deploy is in progress, it skips remediation so it doesn't
# fight an intentional deployment.
#
# The lock is timestamp-based: valid for LOCK_TIMEOUT minutes (default 30).
# A crashed deploy that doesn't release will auto-expire, so FakApp can
# eventually remediate. No PID tracking — the lock is acquired via SSH, so
# the acquiring process exits immediately.
#
# Usage:
#   deploy-lock.sh acquire [reason]   # set the lock (idempotent)
#   deploy-lock.sh release            # remove the lock
#   deploy-lock.sh status             # print "1" if locked, "0" if not
#   deploy-lock.sh owner              # print the lock contents or empty
#
# The lock file lives at /var/run/crowdrelay-deploy.lock (tmpfs, cleared on reboot).

LOCK_FILE="${CROWDRELAY_DEPLOY_LOCK:-/var/run/crowdrelay-deploy.lock}"
LOCK_TIMEOUT="${CROWDRELAY_DEPLOY_LOCK_TIMEOUT:-1800}"  # 30 minutes in seconds

set -euo pipefail

is_stale() {
  [[ -f "$LOCK_FILE" ]] || return 1
  local lock_time now
  lock_time="$(sed -n 's/^epoch=//p' "$LOCK_FILE" 2>/dev/null || echo 0)"
  now="$(date +%s)"
  [[ "$lock_time" =~ ^[0-9]+$ ]] || return 0  # corrupt lock = stale
  (( now - lock_time > LOCK_TIMEOUT ))
}

case "${1:-status}" in
  acquire)
    reason="${2:-deploy}"
    mkdir -p "$(dirname "$LOCK_FILE")" 2>/dev/null || true
    # Auto-release a stale lock before acquiring.
    if is_stale; then rm -f "$LOCK_FILE"; fi
    printf 'reason=%s\nepoch=%s\ntime=%s\n' "$reason" "$(date +%s)" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$LOCK_FILE"
    echo "DEPLOY_LOCK=ACQUIRED reason=$reason"
    ;;
  release)
    rm -f "$LOCK_FILE" 2>/dev/null || true
    echo "DEPLOY_LOCK=RELEASED"
    ;;
  status)
    if [[ -f "$LOCK_FILE" ]] && ! is_stale; then
      echo "1"
    else
      # Clean up a stale lock if found.
      [[ -f "$LOCK_FILE" ]] && rm -f "$LOCK_FILE" 2>/dev/null || true
      echo "0"
    fi
    ;;
  owner)
    if [[ -f "$LOCK_FILE" ]] && ! is_stale; then
      cat "$LOCK_FILE"
    else
      [[ -f "$LOCK_FILE" ]] && rm -f "$LOCK_FILE" 2>/dev/null || true
      echo ""
    fi
    ;;
  *)
    echo "usage: $0 {acquire [reason]|release|status|owner}" >&2
    exit 1
    ;;
esac
