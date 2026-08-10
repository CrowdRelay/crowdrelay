#!/usr/bin/env bash
set -Eeuo pipefail

# Read-only topology audit for the current PostgreSQL container. This script
# deliberately never creates/alters roles or databases: if n8n and CrowdRelay
# share a database/schema today, splitting that state must be planned after the
# lossless PostgreSQL 18 cutover, not guessed during it.

DB_CONTAINER="${DB_CONTAINER:-virya-n8n-cloud-postgres-1}"
DB_ADMIN="${DB_ADMIN:-crowdrelay}"
CROWDRELAY_CONTAINER="${CROWDRELAY_CONTAINER:-crowdrelay-api-1}"
N8N_CONTAINER="${N8N_CONTAINER:-virya-n8n-cloud-n8n-1}"

need() { command -v "$1" >/dev/null 2>&1 || { echo "ERROR: missing command: $1" >&2; exit 1; }; }
need docker

docker inspect "$DB_CONTAINER" >/dev/null 2>&1 || { echo "ERROR: database container not found: $DB_CONTAINER" >&2; exit 1; }
psql_ro() { docker exec "$DB_CONTAINER" psql -X -v ON_ERROR_STOP=1 -U "$DB_ADMIN" "$@"; }

redact_url() {
  python3 -c 'import re,sys; s=sys.stdin.read().strip(); print(re.sub(r"(://[^:/@]+):[^@]*@", r"\\1:<redacted>@", s))' 2>/dev/null || echo '<redacted>'
}
container_env_value() {
  local container="$1" key="$2"
  docker inspect -f '{{range .Config.Env}}{{println .}}{{end}}' "$container" 2>/dev/null \
    | awk -F= -v key="$key" '$1==key {sub(/^[^=]*=/, ""); print; exit}'
}

version="$(psql_ro -d postgres -Atc 'SHOW server_version_num' 2>/dev/null || psql_ro -d "$DB_ADMIN" -Atc 'SHOW server_version_num')"
echo "POSTGRES_TOPOLOGY_AUDIT"
echo "container=$DB_CONTAINER"
echo "server_version_num=$version"
echo

echo '[databases]'
psql_ro -d postgres -P pager=off -c "SELECT datname AS database, pg_get_userbyid(datdba) AS owner, pg_encoding_to_char(encoding) AS encoding, datcollate AS collate FROM pg_database WHERE datallowconn AND NOT datistemplate ORDER BY datname;"
echo

echo '[application roles]'
psql_ro -d postgres -P pager=off -c "SELECT rolname, rolsuper, rolcreatedb, rolcreaterole, rolcanlogin FROM pg_roles WHERE rolname !~ '^pg_' ORDER BY rolname;"
echo

echo '[public schema fingerprints]'
while IFS= read -r db; do
  [[ -n "$db" ]] || continue
  crowdrelay_tables="$(psql_ro -d "$db" -Atc "SELECT count(*) FROM information_schema.tables WHERE table_schema='public' AND table_name IN ('workspaces','fans','area_players','outbox_events','webhook_deliveries')" 2>/dev/null || echo '?')"
  n8n_tables="$(psql_ro -d "$db" -Atc "SELECT count(*) FROM information_schema.tables WHERE table_schema='public' AND table_name IN ('workflow_entity','execution_entity','credentials_entity','settings')" 2>/dev/null || echo '?')"
  echo "database=$db crowdrelay_markers=$crowdrelay_tables/5 n8n_markers=$n8n_tables/4"
done < <(psql_ro -d postgres -Atc "SELECT datname FROM pg_database WHERE datallowconn AND NOT datistemplate ORDER BY datname")
echo

echo '[container connection targets — credentials redacted]'
if docker inspect "$CROWDRELAY_CONTAINER" >/dev/null 2>&1; then
  value="$(container_env_value "$CROWDRELAY_CONTAINER" CROWDRELAY_DATABASE_URL || true)"
  if [[ -n "$value" ]]; then printf 'crowdrelay='; printf '%s' "$value" | redact_url; else echo 'crowdrelay=<CROWDRELAY_DATABASE_URL not present>'; fi
else
  echo "crowdrelay=<container not found: $CROWDRELAY_CONTAINER>"
fi
if docker inspect "$N8N_CONTAINER" >/dev/null 2>&1; then
  db_type="$(container_env_value "$N8N_CONTAINER" DB_TYPE || true)"
  db_host="$(container_env_value "$N8N_CONTAINER" DB_POSTGRESDB_HOST || true)"
  db_name="$(container_env_value "$N8N_CONTAINER" DB_POSTGRESDB_DATABASE || true)"
  db_user="$(container_env_value "$N8N_CONTAINER" DB_POSTGRESDB_USER || true)"
  echo "n8n=type=${db_type:-<unset>} host=${db_host:-<unset>} database=${db_name:-<unset>} user=${db_user:-<unset>}"
else
  echo "n8n=<container not found: $N8N_CONTAINER>"
fi

echo
cat <<'TXT'
[decision]
- If CrowdRelay and n8n already use different databases and roles: preserve that topology during cutover.
- If they use the same role but different databases: split the roles after PG18, then revoke cross-database privileges.
- If n8n tables and CrowdRelay tables share one database/schema: DO NOT auto-split during the major upgrade. First complete/verify PG18, then export/import n8n into a dedicated database in a separate maintenance change.
TXT

echo "POSTGRES_TOPOLOGY_AUDIT=PASS read_only=true"
