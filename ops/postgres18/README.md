# PostgreSQL 18 cutover

CrowdRelay now treats PostgreSQL 18+ as a runtime contract. The production host reported `16.14` in `virya-n8n-cloud-postgres-1`; do **not** fix that by pointing PostgreSQL 18 at the PostgreSQL 16 data directory.

`migrate.sh` uses logical dump/restore into a fresh `postgres:18-alpine` volume. It migrates every non-template database and cluster role, records SHA-256 checksums, compares critical table counts, keeps the PostgreSQL 16 container/volume untouched, and requires an explicit writer list before cutover.

Recommended sequence on the host:

```bash
cd crowdrelay
OLD_CONTAINER=virya-n8n-cloud-postgres-1 OLD_ADMIN=crowdrelay \
  ops/postgres18/migrate.sh preflight

OLD_CONTAINER=virya-n8n-cloud-postgres-1 OLD_ADMIN=crowdrelay \
  ops/postgres18/migrate.sh backup

PG18_ADMIN_PASSWORD='temporary-strong-bootstrap-secret' \
OLD_CONTAINER=virya-n8n-cloud-postgres-1 OLD_ADMIN=crowdrelay \
  ops/postgres18/migrate.sh prepare

PG18_ADMIN_PASSWORD='temporary-strong-bootstrap-secret' \
OLD_CONTAINER=virya-n8n-cloud-postgres-1 OLD_ADMIN=crowdrelay \
  ops/postgres18/migrate.sh restore

PG18_ADMIN_PASSWORD='temporary-strong-bootstrap-secret' \
OLD_CONTAINER=virya-n8n-cloud-postgres-1 OLD_ADMIN=crowdrelay \
  ops/postgres18/migrate.sh verify
```

For the actual switch, provide **all** containers that can write to the cluster. The script refuses to guess them:

```bash
PG18_ADMIN_PASSWORD='temporary-strong-bootstrap-secret' \
WRITER_CONTAINERS='crowdrelay-api-1,crowdrelay-worker-1,<n8n-container>' \
CROWDRELAY_HEALTH_URL='https://signal-api.virya.music/v1/health/ready' \
N8N_HEALTH_URL='https://n8n.virya.music/healthz' \
  ops/postgres18/migrate.sh cutover --cutover
```

If a health probe fails during cutover, the script automatically starts PostgreSQL 16 again and restarts the writer containers. A manual rollback is also available with `rollback --rollback`.

After a stable observation window, remove the PG16 container/volume manually. The migrator deliberately never deletes them.

## n8n and CrowdRelay database isolation

Before cutover, run the read-only topology audit:

```bash
DB_CONTAINER=virya-n8n-cloud-postgres-1 \
  ops/postgres18/audit-topology.sh
```

It reports database owners, application-table fingerprints and the connection targets advertised by the CrowdRelay/n8n containers with passwords redacted. It performs no mutations. Save its output alongside the migration manifest.

The cluster migration preserves existing databases and roles exactly. If n8n already has its own database and role, it stays isolated automatically. If runtime inspection shows both applications use the **same database name/role**, do not split tables during the major-version cutover. First complete the lossless PG18 move, then migrate n8n to a dedicated database in a separate maintenance change; trying to infer n8n's version-specific table set during the PG upgrade increases rollback risk.

## Weekly restore rehearsal

A backup is not considered proven until it restores successfully. After the production backup job has produced a CrowdRelay custom-format `pg_dump`, run the rehearsal against that file:

```bash
ops/postgres18/restore-rehearsal.sh /path/to/latest/crowdrelay.dump
```

The rehearsal creates an isolated `postgres:18-alpine` container with `--network none`, restores with `--no-owner --no-privileges`, verifies SQLx migration `>=37`, checks critical tables and AREA ledger integrity, then removes the disposable container and volume. It never opens a connection to production.

Run this from the server's existing backup scheduler once a week, after the backup has been checksum-verified. Set `KEEP_FAILED_REHEARSAL=1` only during manual diagnosis; the default always cleans the disposable volume.
