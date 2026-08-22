# Production host bootstrap

State that lives on the production host and in no repository. Rebuilding
`virya-crowdrelay` from these repos alone reproduces the containers but not the
items below, and several of them fail in ways that look like something else.

Current production host: `virya-crowdrelay`, Oracle A1.Flex, aarch64, 2 OCPU /
12 GB, reserved IP. It runs the API, worker, rekor anchor, area-management
proxy, both Postgres clusters, the control plane and the single public Caddy
edge. n8n stays on `virya-home` and is published through the edge over
WireGuard `wg0`, the only cross-host link. Tailscale is not used.

## 1. Oracle images ship their own iptables rules

Oracle's Ubuntu image puts a blanket reject in `INPUT` **before** the ufw
chains, so ufw rules for 80, 443 and 51820 have no effect and only SSH reaches
the host. Symptom: certificates never issue and WireGuard never handshakes,
while `ufw status` looks correct and `tcpdump` shows the packets arriving.

```bash
sudo iptables -L INPUT -n --line-numbers        # find the REJECT ... icmp-host-prohibited
sudo iptables -D INPUT <n>
sudo netfilter-persistent save
```

## 2. Postgres superusers

Both clusters bootstrap as `postgres` so a restore never has to drop the role
or the database it is connected as. The applications keep their own roles
(`crowdrelay`, `control_plane`), which the dumps create.

`POSTGRES_USER` only applies on first initialisation of an empty volume. On the
old host the environment claimed `n8n` while the actual superuser was
`virya_admin`, because the volume long predated the environment. Check the
cluster, never the compose file:

```bash
docker exec virya-postgres18 psql -U postgres -Atc \
  'select usename, usesuper from pg_user order by usesuper desc'
```

`pg_dumpall` as a non-superuser fails on `pg_authid`, still exits 0, and writes
a few hundred bytes of partial globals that look like a real dump.

## 3. Edge environment

`${EDGE_ROOT}/edge.env`, mode 0600. See `ops/edge/edge.env.example`. Values
containing `$` must be single quoted.

## 4. Edge Caddy config volume

The `control.virya.music` block injects a bearer read from
`/config/control-plane-admin-token`, which lives in Caddy's `config` **volume**,
not on disk. A fresh host has an empty volume, so every authenticated control
plane call returns 401 while the site itself looks healthy:

```bash
docker run --rm -i -v edge_caddy_config:/c alpine \
  sh -c 'cat > /c/control-plane-admin-token && chmod 600 /c/control-plane-admin-token'
```

## 5. Rekor anchor state

`/var/lib/crowdrelay-rekor` holds the anchor's state and secrets and is not
created by compose. The container runs as `1000:1000`; if Docker creates the
paths itself they are root-owned directories and the anchor dies with
`EACCES ... /data/.write-probe`. Restore the tree, then `chown -R 1000:1000`.

## 6. Heartbeat target

`/opt/crowdrelay/.crowdrelay.local.sh` holds `CROWDRELAY_CONTROL_PLANE_BASE_URL`, which the
heartbeat posts to. It is not in `deploy/.env.production`. Colocated it must be
`http://127.0.0.1:8090`; while it still pointed at the old cross-host WireGuard
address on port 8090 the heartbeat failed silently and the Control Plane showed the
runtime as stale with `apiHealthy: false`.

## 7. n8n on virya-home

n8n binds its own `wg0` WireGuard address on port 5678, so the edge can reach it and
the LAN and internet cannot. It must not depend on the retired `oracle-bridge`.

## Editing bind-mounted files

`sed -i` and `perl -i` replace the inode. A container bind-mounting a single
file keeps the old inode and silently serves stale config; a reload reports
success. Write in place (`cp` over the existing path) or recreate the container.
