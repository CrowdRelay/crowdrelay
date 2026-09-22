#!/usr/bin/env bash
# dev-disk-guard.sh — reclaim what this dev Mac grows. DRY-RUN by default.
#
#   dev-disk-guard.sh            report what would be reclaimed (no changes)
#   dev-disk-guard.sh --apply    actually reclaim it
#
# Companion to ~/.config/devin/scripts/disk-guard.sh, which already reaps cold
# build dirs, caches and /tmp work dirs. This one covers the three classes that
# script does not — see ~/dev/BUILD_AND_DISK_PLAN.md §5:
#
#   1. Stale Postgres test databases in the local dev container: pgt_* test
#      clones and ci_* suite databases older than 24h with no open
#      connections. Never touches crowdrelay*, templates, or anything live.
#   2. Docker: build cache >72h, dangling images, unattached volumes — the
#      VPS guard's rule, unchanged.
#   3. Abandoned worktree targets: target/ under .claude/worktrees/* and
#      ~/dev/.worktrees/* whose git worktree is gone or untouched for 7 days.
#      The worktree itself is never removed — only the rebuildable output.
#   4. Report: every pass appends per-class freed bytes to the log.
#
# Log: ~/.config/devin/dev-disk-guard.log
# Installed hourly by ~/Library/LaunchAgents/dev.dev-disk-guard.plist.

set -uo pipefail

APPLY=0
[ "${1:-}" = "--apply" ] && APPLY=1

LOG="$HOME/.config/devin/dev-disk-guard.log"
AGE_HOURS="${DEV_GUARD_AGE_HOURS:-24}"
WORKTREE_AGE_DAYS="${DEV_GUARD_WORKTREE_AGE_DAYS:-7}"
PG_CONTAINER="${DEV_GUARD_PG_CONTAINER:-crowdrelay-postgres-1}"
PG_USER="${DEV_GUARD_PG_USER:-crowdrelay}"

log() { printf '%s %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$*" >>"$LOG"; }

freed_kb=0
note() {  # label, kb-or-0 freed, detail
  log "  $1: $3 (freed_kb=$2)"
  freed_kb=$((freed_kb + $2))
}

mode="dry-run"
[ "$APPLY" -eq 1 ] && mode="apply"
# / is the sealed system volume on macOS — the data lives on /System/Volumes/Data.
used_pct=$(df -h /System/Volumes/Data | awk 'NR==2 {gsub("%","",$5); print $5}')
log "--- pass start ($mode, disk ${used_pct}%) ---"

# ── 1. stale postgres test databases ────────────────────────────────────────
if docker inspect "$PG_CONTAINER" >/dev/null 2>&1; then
  psql() { docker exec -i "$PG_CONTAINER" psql -U "$PG_USER" -d postgres -At "$@"; }
  # pgt_* and crowdrelay_<tag>_<uuid>: age is in the name — uuid v7 leading
  # 48 bits are unix ms. crowdrelay_* is the previous clone naming; nothing
  # creates it anymore but the strays still occupy disk. Named databases like
  # crowdrelay_agents_boundary_test have no uuid suffix, so the regex cannot
  # match them and they are never swept.
  # ci_*: no timestamp in the name, so age is the database dir's mtime.
  stale_dbs=$(psql -c "
    SELECT datname FROM (
      SELECT d.datname,
        CASE
          WHEN d.datname LIKE 'pgt\_%' THEN
            now() - to_timestamp(
              ('x' || lpad(substring(d.datname from 'pgt_[0-9a-f]+_([0-9a-f]{12})'), 16, '0')
              )::bit(64)::bigint / 1000.0)
          WHEN d.datname LIKE 'crowdrelay\_%\_%' THEN
            now() - to_timestamp(
              ('x' || lpad(substring(d.datname from '_([0-9a-f]{12})[0-9a-f]{20}\$'), 16, '0')
              )::bit(64)::bigint / 1000.0)
          ELSE
            now() - (pg_stat_file('base/' || d.oid || '/PG_VERSION')).modification
        END AS age,
        EXISTS (SELECT 1 FROM pg_stat_activity a WHERE a.datname = d.datname) AS has_conn
      FROM pg_database d
      WHERE (d.datname LIKE 'pgt\_%'
          OR d.datname LIKE 'ci\_%'
          OR (d.datname LIKE 'crowdrelay\_%\_%'
              AND substring(d.datname from '_([0-9a-f]{32})\$') IS NOT NULL))
        AND d.datistemplate = false
    ) s
    WHERE age > interval '1 hour' * $AGE_HOURS AND NOT has_conn" 2>/dev/null)
  for db in $stale_dbs; do
    size_kb=$(psql -c "SELECT pg_database_size('$db') / 1024" 2>/dev/null || echo 0)
    if [ "$APPLY" -eq 1 ]; then
      psql -c "DROP DATABASE \"$db\"" >/dev/null 2>&1 && note "pg-drop" "${size_kb:-0}" "$db"
    else
      note "pg-drop(dry)" 0 "$db (${size_kb:-0} KB)"
    fi
  done
else
  log "  pg: container $PG_CONTAINER absent — skipped"
fi

# ── 2. docker: cache >72h, dangling images, unattached volumes ──────────────
if docker info >/dev/null 2>&1; then
  before=$(docker system df --format '{{.Reclaimable}}' 2>/dev/null | head -1)
  if [ "$APPLY" -eq 1 ]; then
    docker builder prune --filter until=72h -f >/dev/null 2>&1
    docker image prune -f >/dev/null 2>&1
    docker volume prune -f >/dev/null 2>&1
    after=$(docker system df --format '{{.Reclaimable}}' 2>/dev/null | head -1)
    log "  docker: reclaimable ${before:-?} -> ${after:-?}"
  else
    log "  docker(dry): reclaimable now: ${before:-?}"
  fi
else
  log "  docker: not running — skipped"
fi

# ── 3. abandoned worktree target dirs ───────────────────────────────────────
for wtroot in "$HOME"/dev/*/.claude/worktrees "$HOME"/dev/.worktrees; do
  [ -d "$wtroot" ] || continue
  for wt in "$wtroot"/*/; do
    [ -d "$wt/target" ] || continue
    wt=${wt%/}
    # Gone = the worktree's own git metadata no longer resolves (the repo's
    # .git/worktrees/<name> entry was pruned). Asking the worktree itself
    # works no matter which repo it belongs to.
    gone=0
    git -C "$wt" rev-parse --git-dir >/dev/null 2>&1 || gone=1
    cold=0
    [ -z "$(find "$wt" -newermt "$WORKTREE_AGE_DAYS days ago" -print -quit 2>/dev/null)" ] && cold=1
    if [ "$gone" -eq 0 ] && [ "$cold" -eq 0 ]; then continue; fi
    size_kb=$(du -sk "$wt/target" 2>/dev/null | cut -f1)
    if [ "$APPLY" -eq 1 ]; then
      rm -rf "$wt/target" && note "worktree-target" "${size_kb:-0}" "$wt (gone=$gone cold=$cold)"
    else
      note "worktree-target(dry)" 0 "$wt (${size_kb:-0} KB gone=$gone cold=$cold)"
    fi
  done
done

log "--- pass end ($mode, freed $((freed_kb / 1024)) MB) ---"
echo "dev-disk-guard $mode done — see $LOG"
