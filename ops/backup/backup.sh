#!/usr/bin/env bash
set -Eeuo pipefail

# Nightly logical backup of the authoritative CrowdRelay PostgreSQL database.
# Produces a checksummed, atomically renamed custom-format dump, prunes old
# generations, and can chain into the existing restore rehearsal. On failure
# it posts one rate-limited alert when ALERT_WEBHOOK_URL is configured.
#
# Designed to run as deploy/systemd/virya-crowdrelay-backup.timer and to be
# callable directly, or as the crowdrelayctl `crowdrelay_backup` hook.

log() { printf '[backup] %s\n' "$*"; }
die() { printf '[backup] ERROR: %s\n' "$*" >&2; exit 1; }

CONTAINER="${CROWDRELAY_DB_CONTAINER:-postgres}"
BACKUP_DIR="${CROWDRELAY_BACKUP_DIR:-/var/backups/crowdrelay}"
RETAIN_DAYS="${CROWDRELAY_BACKUP_RETAIN_DAYS:-14}"
PG_USER="${POSTGRES_USER:-crowdrelay}"
PG_DATABASE="${POSTGRES_DB:-crowdrelay}"
VERIFY="${CROWDRELAY_BACKUP_VERIFY:-1}"
ALERT_WEBHOOK_URL="${ALERT_WEBHOOK_URL:-}"
ALERT_COOLDOWN_SECONDS="${ALERT_COOLDOWN_SECONDS:-3600}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

notify_failure() {
  local message="crowdrelay backup FAILED on $(hostname -s 2>/dev/null || echo unknown) at $(date -u +%FT%TZ)"
  [[ -n "$ALERT_WEBHOOK_URL" ]] || return 0
  local state_dir="${CROWDRELAY_BACKUP_STATE_DIR:-${TMPDIR:-/tmp}/crowdrelay-backup}"
  mkdir -p "$state_dir"
  local last_alert=0
  [[ -r "$state_dir/last-alert-at" ]] && last_alert="$(cat "$state_dir/last-alert-at" 2>/dev/null || echo 0)"
  local now; now="$(date +%s)"
  (( now - last_alert >= ALERT_COOLDOWN_SECONDS )) || return 0
  printf '%s\n' "$now" > "$state_dir/last-alert-at"
  curl --silent --show-error --max-time 10 \
    -H 'content-type: application/json' \
    -d "$(printf '{"content":%s}' "$(printf '%s' "$message" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))')" )" \
    "$ALERT_WEBHOOK_URL" >/dev/null || true
}
trap notify_failure ERR

need() { command -v "$1" >/dev/null 2>&1 || die "required command missing: $1"; }
need docker
need sha256sum
need date

docker inspect "$CONTAINER" >/dev/null 2>&1 \
  || die "database container not found: $CONTAINER"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
TARGET="$BACKUP_DIR/crowdrelay-$STAMP.dump"
PARTIAL="$TARGET.partial"
mkdir -p "$BACKUP_DIR"

log "dumping $PG_DATABASE from container $CONTAINER"
docker exec "$CONTAINER" pg_dump --username "$PG_USER" --dbname "$PG_DATABASE" \
  --format=custom --no-owner --no-privileges \
  > "$PARTIAL" || { rm -f "$PARTIAL"; die "pg_dump failed"; }

[[ -s "$PARTIAL" ]] || { rm -f "$PARTIAL"; die "pg_dump produced an empty stream"; }
mv "$PARTIAL" "$TARGET"
CHECKSUM="$(sha256sum "$TARGET" | awk '{print $1}')"
printf '%s  %s\n' "$CHECKSUM" "$(basename "$TARGET")" > "$TARGET.sha256"
SIZE_BYTES="$(wc -c < "$TARGET" | tr -d ' ')"
log "wrote $(basename "$TARGET") bytes=$SIZE_BYTES sha256=$CHECKSUM"

PRUNED_BEFORE="$(ls -1 "$BACKUP_DIR"/crowdrelay-*.dump 2>/dev/null | wc -l | tr -d ' ')"
find "$BACKUP_DIR" -name 'crowdrelay-*.dump' -type f -mtime "+$RETAIN_DAYS" -delete
find "$BACKUP_DIR" -name 'crowdrelay-*.dump.sha256' -type f -mtime "+$RETAIN_DAYS" -delete
PRUNED_AFTER="$(ls -1 "$BACKUP_DIR"/crowdrelay-*.dump 2>/dev/null | wc -l | tr -d ' ')"
(( PRUNED_BEFORE != PRUNED_AFTER )) && log "pruned $(( PRUNED_BEFORE - PRUNED_AFTER )) generation(s); retaining $PRUNED_AFTER"

if [[ "$VERIFY" == "1" ]]; then
  if [[ -x "$SCRIPT_DIR/restore-rehearsal.sh" ]] && command -v docker >/dev/null; then
    log "chaining restore rehearsal on the fresh dump"
    "$SCRIPT_DIR/restore-rehearsal.sh" "$TARGET" \
      && log "restore rehearsal PASSED" \
      || die "restore rehearsal FAILED for $TARGET"
  else
    log "restore-rehearsal.sh unavailable; skipping verification"
  fi
fi

log "OK retention_days=$RETAIN_DAYS dir=$BACKUP_DIR"
