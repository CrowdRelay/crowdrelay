#!/usr/bin/env bash
set -Eeuo pipefail

# Prove that a logical CrowdRelay backup can be restored into PostgreSQL 18.
# This script never connects to the production database. It creates an isolated
# disposable Docker volume/container and removes both on exit.

DUMP_FILE="${1:-${DUMP_FILE:-}}"
PG18_IMAGE="${PG18_IMAGE:-postgres:18-alpine}"
REHEARSAL_DB="${REHEARSAL_DB:-crowdrelay_restore_rehearsal}"
CONTAINER="${REHEARSAL_CONTAINER:-virya-pg18-restore-rehearsal-$$}"
VOLUME="${REHEARSAL_VOLUME:-${CONTAINER}-data}"
PASSWORD="${REHEARSAL_PASSWORD:-rehearsal-only-not-routable}"
KEEP_FAILED="${KEEP_FAILED_REHEARSAL:-0}"

log() { printf '[restore-rehearsal] %s\n' "$*"; }
die() { printf '[restore-rehearsal] ERROR: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "required command missing: $1"; }
need docker
need sha256sum

[[ -n "$DUMP_FILE" ]] || die "usage: $0 /path/to/crowdrelay.dump (or DUMP_FILE=...)"
[[ -r "$DUMP_FILE" && -s "$DUMP_FILE" ]] || die "backup is not readable/non-empty: $DUMP_FILE"

result=1
cleanup() {
  if (( result != 0 )) && [[ "$KEEP_FAILED" == 1 ]]; then
    log "FAILED_REHEARSAL_PRESERVED container=$CONTAINER volume=$VOLUME"
    return
  fi
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  docker volume rm "$VOLUME" >/dev/null 2>&1 || true
}
trap cleanup EXIT

checksum="$(sha256sum "$DUMP_FILE" | awk '{print $1}')"
[[ "$checksum" =~ ^[0-9a-f]{64}$ ]] || die "could not calculate SHA-256"
log "backup_sha256=$checksum bytes=$(stat -c %s "$DUMP_FILE")"

docker image inspect "$PG18_IMAGE" >/dev/null 2>&1 || docker pull "$PG18_IMAGE" >/dev/null
docker volume create "$VOLUME" >/dev/null
docker run -d \
  --name "$CONTAINER" \
  --network none \
  --shm-size 256m \
  -e POSTGRES_USER=postgres \
  -e POSTGRES_PASSWORD="$PASSWORD" \
  -e POSTGRES_DB=postgres \
  -v "$VOLUME:/var/lib/postgresql" \
  "$PG18_IMAGE" >/dev/null

for _ in $(seq 1 45); do
  if docker exec -e PGPASSWORD="$PASSWORD" "$CONTAINER" pg_isready -U postgres -d postgres >/dev/null 2>&1; then
    break
  fi
  sleep 1
done
docker exec -e PGPASSWORD="$PASSWORD" "$CONTAINER" pg_isready -U postgres -d postgres >/dev/null \
  || die "PostgreSQL 18 rehearsal instance did not become ready"

version="$(docker exec -e PGPASSWORD="$PASSWORD" "$CONTAINER" psql -X -At -U postgres -d postgres -c 'SHOW server_version_num')"
[[ "$version" =~ ^[0-9]+$ ]] && (( version >= 180000 )) || die "rehearsal runtime is not PostgreSQL 18+: $version"

docker exec -e PGPASSWORD="$PASSWORD" "$CONTAINER" createdb -U postgres "$REHEARSAL_DB"
docker cp "$DUMP_FILE" "$CONTAINER:/tmp/backup.dump"
# --no-owner/--no-privileges makes the rehearsal independent of production role
# names while still exercising schema, data, indexes, constraints and types.
docker exec -e PGPASSWORD="$PASSWORD" "$CONTAINER" \
  pg_restore -U postgres -d "$REHEARSAL_DB" --no-owner --no-privileges --exit-on-error /tmp/backup.dump >/dev/null

docker exec -e PGPASSWORD="$PASSWORD" "$CONTAINER" \
  psql -X -v ON_ERROR_STOP=1 -U postgres -d "$REHEARSAL_DB" -c 'ANALYZE;' >/dev/null

sql() {
  docker exec -e PGPASSWORD="$PASSWORD" "$CONTAINER" \
    psql -X -At -v ON_ERROR_STOP=1 -U postgres -d "$REHEARSAL_DB" -c "$1"
}

migration="$(sql "SELECT COALESCE(max(version),0) FROM _sqlx_migrations" 2>/dev/null || echo 0)"
[[ "$migration" =~ ^[0-9]+$ ]] || die "invalid SQLx migration version: $migration"
(( migration >= 39 )) || die "backup schema is older than required migration 39: $migration"

for table in workspaces fans area_players area_claims area_credit_ledger area_reward_vouchers area_ticket_rewards outbox_events webhook_deliveries; do
  exists="$(sql "SELECT to_regclass('public.${table}') IS NOT NULL")"
  [[ "$exists" == t ]] || die "required table missing after restore: $table"
  count="$(sql "SELECT count(*) FROM public.\"${table}\"")"
  [[ "$count" =~ ^[0-9]+$ ]] || die "table count failed: $table"
  log "table=$table rows=$count"
done

# Integrity checks that are cheap enough for a weekly rehearsal and catch the
# most dangerous wallet/ticket corruption without mutating restored data.
negative_balances="$(sql "SELECT count(*) FROM (SELECT player_id, sum(delta) balance FROM area_credit_ledger GROUP BY player_id HAVING sum(delta) < 0) q")"
(( negative_balances == 0 )) || die "negative AREA balances detected: $negative_balances"
invalid_ticket_rewards="$(sql "SELECT count(*) FROM area_ticket_rewards WHERE status='issued' AND (public_reference IS NULL OR issued_at IS NULL)")"
(( invalid_ticket_rewards == 0 )) || die "invalid issued AREA ticket rewards: $invalid_ticket_rewards"

result=0
log "RESTORE_REHEARSAL=PASS postgres=$version migration=$migration isolated=true"
