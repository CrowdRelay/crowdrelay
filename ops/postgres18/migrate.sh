#!/usr/bin/env bash
set -Eeuo pipefail

# PostgreSQL 16 -> 18 cluster migration for the Virya/CrowdRelay host.
# Safe default: read-only preflight. Destructive/cutover phases require explicit
# flags and never delete the old PG16 container or its volume.

PHASE="preflight"
CONFIRM_CUTOVER=0
CONFIRM_ROLLBACK=0
for arg in "$@"; do
  case "$arg" in
    preflight|backup|prepare|restore|verify|cutover|rollback) PHASE="$arg" ;;
    --cutover) CONFIRM_CUTOVER=1 ;;
    --rollback) CONFIRM_ROLLBACK=1 ;;
    -h|--help)
      cat <<'EOF'
Usage: ops/postgres18/migrate.sh [preflight|backup|prepare|restore|verify|cutover|rollback] [--cutover|--rollback]

Environment (important):
  OLD_CONTAINER       default: virya-n8n-cloud-postgres-1
  OLD_ADMIN           default: crowdrelay
  NEW_CONTAINER       default: virya-postgres18
  NEW_VOLUME          default: virya-postgres18-data
  PG18_IMAGE          default: postgres:18-alpine
  PG18_ADMIN_PASSWORD required for prepare/restore/cutover
  STATE_DIR           persistent migration state; default: $HOME/virya-pg18-migration
  DOCKER_NETWORK      optional; autodetected from OLD_CONTAINER
  OLD_DB_ALIAS        default: postgres
  NEW_DB_ALIAS        default: postgres18
  WRITER_CONTAINERS   comma/space-separated API/worker/n8n containers; REQUIRED for cutover
  CROWDRELAY_HEALTH_URL optional e.g. https://signal-api.virya.music/v1/health/ready
  N8N_HEALTH_URL        optional n8n health endpoint

The old PG16 container/volume is never removed. `cutover --cutover` stops all
writers, takes a final backup, restores it into PG18, verifies data, switches
the `postgres` network alias, then restarts writers. On failed smoke checks it
rolls back automatically.
EOF
      exit 0 ;;
    *) echo "Unknown argument: $arg" >&2; exit 2 ;;
  esac
done

OLD_CONTAINER="${OLD_CONTAINER:-virya-n8n-cloud-postgres-1}"
OLD_ADMIN="${OLD_ADMIN:-crowdrelay}"
NEW_CONTAINER="${NEW_CONTAINER:-virya-postgres18}"
NEW_VOLUME="${NEW_VOLUME:-virya-postgres18-data}"
PG18_IMAGE="${PG18_IMAGE:-postgres:18-alpine}"
STATE_DIR="${STATE_DIR:-$HOME/virya-pg18-migration}"
OLD_DB_ALIAS="${OLD_DB_ALIAS:-postgres}"
NEW_DB_ALIAS="${NEW_DB_ALIAS:-postgres18}"
MANIFEST="$STATE_DIR/manifest.tsv"
GLOBALS="$STATE_DIR/globals.sql"
COUNTS_BEFORE="$STATE_DIR/counts.before.tsv"
COUNTS_AFTER="$STATE_DIR/counts.after.tsv"
CUTOVER_MARKER="$STATE_DIR/cutover.completed"
mkdir -p "$STATE_DIR"
chmod 700 "$STATE_DIR"

log() { printf '[pg18] %s\n' "$*"; }
die() { printf '[pg18] ERROR: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "required command missing: $1"; }
need docker
need sha256sum

container_running() { [[ "$(docker inspect -f '{{.State.Running}}' "$1" 2>/dev/null || true)" == "true" ]]; }
old_psql() { docker exec "$OLD_CONTAINER" psql -X -v ON_ERROR_STOP=1 -U "$OLD_ADMIN" "$@"; }
new_psql() { docker exec -e PGPASSWORD="${PG18_ADMIN_PASSWORD:-}" "$NEW_CONTAINER" psql -X -v ON_ERROR_STOP=1 -U "$OLD_ADMIN" -d postgres18_admin "$@"; }
network_name() {
  if [[ -n "${DOCKER_NETWORK:-}" ]]; then printf '%s' "$DOCKER_NETWORK"; return; fi
  docker inspect -f '{{range $name, $_ := .NetworkSettings.Networks}}{{println $name}}{{end}}' "$OLD_CONTAINER" | head -n1
}
list_databases() {
  old_psql -d postgres -Atc "SELECT datname FROM pg_database WHERE datallowconn AND NOT datistemplate ORDER BY datname" 2>/dev/null \
    || old_psql -d "$OLD_ADMIN" -Atc "SELECT datname FROM pg_database WHERE datallowconn AND NOT datistemplate ORDER BY datname"
}
record_counts() {
  local target="$1" out="$2"
  : > "$out"
  local db
  while IFS= read -r db; do
    [[ -n "$db" ]] || continue
    local runner
    if [[ "$target" == old ]]; then
      runner=(docker exec "$OLD_CONTAINER" psql -X -U "$OLD_ADMIN" -d "$db" -At)
    else
      runner=(docker exec -e PGPASSWORD="${PG18_ADMIN_PASSWORD:-}" "$NEW_CONTAINER" psql -X -U "$OLD_ADMIN" -d "$db" -At)
    fi
    local table_count
    table_count="$(${runner[@]} -c "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE c.relkind='r' AND n.nspname NOT IN ('pg_catalog','information_schema')" 2>/dev/null || echo ERR)"
    printf '%s\t__table_count__\t%s\n' "$db" "$table_count" >> "$out"
    for table in workspaces fans area_players area_claims area_reward_vouchers area_ticket_rewards outbox_events webhook_deliveries workflow_entity execution_entity credentials_entity; do
      local exists
      exists="$(${runner[@]} -c "SELECT to_regclass('public.${table}') IS NOT NULL" 2>/dev/null || echo f)"
      if [[ "$exists" == t ]]; then
        local count
        count="$(${runner[@]} -c "SELECT count(*) FROM public.\"${table}\"" 2>/dev/null || echo ERR)"
        printf '%s\t%s\t%s\n' "$db" "$table" "$count" >> "$out"
      fi
    done
  done < <(if [[ "$target" == old ]]; then list_databases; else new_psql -Atc "SELECT datname FROM pg_database WHERE datallowconn AND NOT datistemplate AND datname <> 'postgres18_admin' ORDER BY datname"; fi)
}

preflight() {
  container_running "$OLD_CONTAINER" || die "old PostgreSQL container is not running: $OLD_CONTAINER"
  local version super network
  version="$(old_psql -d "$OLD_ADMIN" -Atc 'SHOW server_version_num' 2>/dev/null || old_psql -d postgres -Atc 'SHOW server_version_num')"
  [[ "$version" =~ ^[0-9]+$ ]] || die "could not determine old PostgreSQL server_version_num"
  (( version < 180000 )) || die "old runtime is already PostgreSQL 18+ ($version); refusing a 16->18 migration"
  super="$(old_psql -d "$OLD_ADMIN" -Atc "SELECT rolsuper FROM pg_roles WHERE rolname = current_user" 2>/dev/null || true)"
  [[ "$super" == t ]] || die "OLD_ADMIN=$OLD_ADMIN must be a superuser for full-cluster roles/databases migration"
  network="$(network_name)"
  [[ -n "$network" ]] || die "could not detect Docker network; set DOCKER_NETWORK"
  log "old=$OLD_CONTAINER server_version_num=$version admin=$OLD_ADMIN network=$network"
  log "databases: $(list_databases | paste -sd, -)"
  old_psql -d "$OLD_ADMIN" -Atc "SELECT extname || '=' || extversion FROM pg_extension ORDER BY extname" > "$STATE_DIR/extensions.before.txt" 2>/dev/null || true
  record_counts old "$COUNTS_BEFORE"
  log "PREFLIGHT=PASS state=$STATE_DIR"
}

backup_cluster() {
  preflight >/dev/null
  rm -f "$STATE_DIR"/*.dump "$GLOBALS" "$MANIFEST"
  log "dumping cluster globals"
  docker exec "$OLD_CONTAINER" pg_dumpall -U "$OLD_ADMIN" --globals-only > "$GLOBALS"
  [[ -s "$GLOBALS" ]] || die "globals dump is empty"
  local db
  while IFS= read -r db; do
    [[ -n "$db" ]] || continue
    local file="$STATE_DIR/${db}.dump"
    log "dumping database=$db"
    docker exec "$OLD_CONTAINER" pg_dump -U "$OLD_ADMIN" -Fc -C --no-acl "$db" > "$file"
    [[ -s "$file" ]] || die "database dump is empty: $db"
    printf '%s\t%s\t%s\n' "$db" "$(sha256sum "$file" | awk '{print $1}')" "$(stat -c %s "$file")" >> "$MANIFEST"
  done < <(list_databases)
  sha256sum "$GLOBALS" > "$STATE_DIR/globals.sha256"
  sha256sum -c "$STATE_DIR/globals.sha256" >/dev/null
  while IFS=$'\t' read -r db checksum _; do echo "$checksum  $STATE_DIR/${db}.dump"; done < "$MANIFEST" | sha256sum -c - >/dev/null
  record_counts old "$COUNTS_BEFORE"
  log "BACKUP=PASS files=$(wc -l < "$MANIFEST" | tr -d ' ')"
}

prepare_pg18() {
  [[ -n "${PG18_ADMIN_PASSWORD:-}" ]] || die "PG18_ADMIN_PASSWORD is required"
  local network; network="$(network_name)"
  docker image inspect "$PG18_IMAGE" >/dev/null 2>&1 || docker pull "$PG18_IMAGE" >/dev/null
  if docker inspect "$NEW_CONTAINER" >/dev/null 2>&1; then
    container_running "$NEW_CONTAINER" || docker start "$NEW_CONTAINER" >/dev/null
  else
    docker volume create "$NEW_VOLUME" >/dev/null
    docker run -d \
      --name "$NEW_CONTAINER" \
      --restart unless-stopped \
      --network "$network" \
      --network-alias "$NEW_DB_ALIAS" \
      --shm-size 256m \
      -e POSTGRES_USER="$OLD_ADMIN" \
      -e POSTGRES_PASSWORD="$PG18_ADMIN_PASSWORD" \
      -e POSTGRES_DB=postgres18_admin \
      -v "$NEW_VOLUME:/var/lib/postgresql" \
      "$PG18_IMAGE" \
      -c io_method=worker \
      -c io_workers="${PG18_IO_WORKERS:-3}" \
      -c effective_io_concurrency="${PG18_EFFECTIVE_IO_CONCURRENCY:-16}" \
      -c maintenance_io_concurrency="${PG18_MAINTENANCE_IO_CONCURRENCY:-16}" >/dev/null
  fi
  for _ in $(seq 1 40); do
    if new_psql -Atc 'SELECT 1' >/dev/null 2>&1; then break; fi
    sleep 1
  done
  local version; version="$(new_psql -Atc 'SHOW server_version_num')"
  (( version >= 180000 )) || die "new runtime is not PostgreSQL 18+: $version"
  log "PREPARE=PASS new=$NEW_CONTAINER version=$version volume=$NEW_VOLUME alias=$NEW_DB_ALIAS"
}

restore_cluster() {
  [[ -s "$GLOBALS" && -s "$MANIFEST" ]] || die "backup is missing; run backup first"
  prepare_pg18 >/dev/null
  local filtered="$STATE_DIR/globals.filtered.sql"
  # The bootstrap superuser already exists in the new cluster. Keep its ALTER
  # statements (including the original SCRAM verifier), but skip only CREATE ROLE.
  awk -v role="$OLD_ADMIN" '$0 != "CREATE ROLE " role ";" { print }' "$GLOBALS" > "$filtered"
  docker cp "$filtered" "$NEW_CONTAINER:/tmp/globals.sql"
  new_psql -f /tmp/globals.sql >/dev/null

  local db checksum _ file
  while IFS=$'\t' read -r db checksum _; do
    file="$STATE_DIR/${db}.dump"
    [[ "$(sha256sum "$file" | awk '{print $1}')" == "$checksum" ]] || die "checksum mismatch: $db"
    log "restoring database=$db"
    # Make the phase repeatable: remove an earlier restored copy, never touching
    # the migration admin database.
    new_psql -c "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '$db' AND pid <> pg_backend_pid();" >/dev/null
    new_psql -c "DROP DATABASE IF EXISTS \"$db\";" >/dev/null
    docker cp "$file" "$NEW_CONTAINER:/tmp/${db}.dump"
    docker exec -e PGPASSWORD="$PG18_ADMIN_PASSWORD" "$NEW_CONTAINER" \
      pg_restore -U "$OLD_ADMIN" -d postgres18_admin --create --no-acl --exit-on-error "/tmp/${db}.dump" >/dev/null
    docker exec "$NEW_CONTAINER" rm -f "/tmp/${db}.dump"
    docker exec -e PGPASSWORD="$PG18_ADMIN_PASSWORD" "$NEW_CONTAINER" \
      psql -X -U "$OLD_ADMIN" -d "$db" -v ON_ERROR_STOP=1 -c 'ANALYZE;' >/dev/null
  done < "$MANIFEST"
  record_counts new "$COUNTS_AFTER"
  log "RESTORE=PASS"
}

verify_cluster() {
  [[ -s "$COUNTS_BEFORE" ]] || die "pre-migration counts missing"
  container_running "$NEW_CONTAINER" || die "new PostgreSQL container is not running"
  record_counts new "$COUNTS_AFTER"
  if ! diff -u "$COUNTS_BEFORE" "$COUNTS_AFTER"; then
    die "critical table counts differ after restore"
  fi
  local version; version="$(new_psql -Atc 'SHOW server_version_num')"
  (( version >= 180000 )) || die "new PostgreSQL version check failed: $version"
  new_psql -Atc 'SELECT 1' >/dev/null
  log "VERIFY=PASS version=$version counts=identical"
}

parse_writers() {
  printf '%s\n' "${WRITER_CONTAINERS:-}" | tr ', ' '\n\n' | sed '/^$/d'
}
stop_writers() {
  local writers; writers="$(parse_writers)"
  [[ -n "$writers" ]] || die "WRITER_CONTAINERS is required for cutover; refusing to guess writers"
  printf '%s\n' "$writers" > "$STATE_DIR/writers.txt"
  while IFS= read -r c; do
    docker inspect "$c" >/dev/null 2>&1 || die "writer container not found: $c"
    container_running "$c" && docker stop --time 45 "$c" >/dev/null
  done <<< "$writers"
}
start_writers() {
  [[ -s "$STATE_DIR/writers.txt" ]] || return 0
  while IFS= read -r c; do docker start "$c" >/dev/null; done < "$STATE_DIR/writers.txt"
}
smoke() {
  local failed=0
  if [[ -n "${CROWDRELAY_HEALTH_URL:-}" ]]; then
    curl --proto '=https' --tlsv1.2 --fail --silent --show-error --max-time 15 "$CROWDRELAY_HEALTH_URL" >/dev/null || failed=1
  fi
  if [[ -n "${N8N_HEALTH_URL:-}" ]]; then
    curl --proto '=https' --tlsv1.2 --fail --silent --show-error --max-time 15 "$N8N_HEALTH_URL" >/dev/null || failed=1
  fi
  return "$failed"
}

rollback_cutover() {
  (( CONFIRM_ROLLBACK == 1 )) || die "rollback requires --rollback"
  log "stopping writers for rollback"
  [[ -s "$STATE_DIR/writers.txt" ]] && while IFS= read -r c; do container_running "$c" && docker stop --time 45 "$c" >/dev/null || true; done < "$STATE_DIR/writers.txt"
  container_running "$NEW_CONTAINER" && docker stop --time 45 "$NEW_CONTAINER" >/dev/null || true
  docker start "$OLD_CONTAINER" >/dev/null
  start_writers
  rm -f "$CUTOVER_MARKER"
  log "ROLLBACK=PASS old_pg16=running new_pg18=stopped"
}

cutover() {
  (( CONFIRM_CUTOVER == 1 )) || die "cutover requires explicit --cutover"
  [[ -n "${PG18_ADMIN_PASSWORD:-}" ]] || die "PG18_ADMIN_PASSWORD is required"
  preflight >/dev/null
  prepare_pg18 >/dev/null
  stop_writers
  trap 'log "cutover failed; attempting automatic rollback"; CONFIRM_ROLLBACK=1; rollback_cutover || true' ERR
  # Final consistent snapshot after all application writers are stopped.
  backup_cluster >/dev/null
  restore_cluster >/dev/null
  verify_cluster >/dev/null

  local network; network="$(network_name)"
  docker stop --time 60 "$OLD_CONTAINER" >/dev/null
  # Reattach PG18 so `postgres` resolves to exactly one live container.
  docker network disconnect "$network" "$NEW_CONTAINER" >/dev/null 2>&1 || true
  docker network connect --alias "$OLD_DB_ALIAS" --alias "$NEW_DB_ALIAS" "$network" "$NEW_CONTAINER"
  start_writers
  if ! smoke; then
    die "post-cutover health check failed"
  fi
  date -u +%FT%TZ > "$CUTOVER_MARKER"
  trap - ERR
  log "CUTOVER=PASS pg18=$NEW_CONTAINER old_pg16=$OLD_CONTAINER old_volume_preserved=yes"
}

case "$PHASE" in
  preflight) preflight ;;
  backup) backup_cluster ;;
  prepare) prepare_pg18 ;;
  restore) restore_cluster ;;
  verify) verify_cluster ;;
  cutover) cutover ;;
  rollback) rollback_cutover ;;
esac
