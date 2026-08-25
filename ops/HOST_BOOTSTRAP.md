# Host bootstrap notes — virya-crowdrelay

State that lives only on the host, not in any repo's compose files. Read before
rebuilding the box or touching the control-plane stack.

## Control plane stack (`/srv/crowdrelay-control-plane`)

Bring-up is **two files, always**:

```sh
cd /srv/crowdrelay-control-plane
docker compose -f compose.production.yml -f compose.area.yml up -d
```

- Base-only `up -d` silently drops the shared `area-management` network alias
  (`control-plane`), so `virya-edge-caddy` 503s on control.virya.music, and
  loses the tunnel allowlist sidecar that every deploy gate verifies.
- Database: **postgres:18-alpine** on volume `control-plane-pg` until 19 GA
  (expected Sep/Oct 2026; dev/CI already run 19beta3 via
  `CROWDRELAY_POSTGRES_IMAGE`). Do not swap image or volume by hand.
- The app talks to CrowdRelay management through
  `virya-area-tunnel` (Caddy allowlist on `127.0.0.1:18080`, shares the app
  network namespace). It is an authority filter, not legacy transport: only
  whitelisted `/v1/control-plane/*` routes pass. Deploy gates refuse to run
  while it is down.

### Postgres 19 migration recipe (when 19 GA lands)

1. `docker compose exec postgres pg_dump -U postgres control_plane | gzip > backups/control-plane-pre-pg19-$(date +%Y%m%d).sql.gz`
2. Point `compose.production.yml` at `postgres:19-alpine` with a NEW volume.
3. `docker compose up -d postgres`, restore the dump into it, then start app.
4. Verify `/api/v1/overview` over the edge before removing the old volume.

Reference dump taken 2026-08-25:
`backups/control-plane-pre-pg19-manual-20260825.sql.gz`.

## History

- 2026-08-23: consolidated from virya-home onto this host; control plane was
  fully removed from virya-home (final pre-removal dump in
  `/srv/crowdrelay-control-plane/backups/`).
