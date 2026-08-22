# Public edge

The only stack that binds 80/443 on the production host. Terminates TLS for
`n8n.virya.music`, `signal-api.virya.music` and `control.virya.music`, and runs
the CrowdRelay to n8n event bridge.

| File | Role |
|---|---|
| `compose.edge.yaml` | Caddy + `crowdrelay-n8n-bridge` |
| `Caddyfile` | routing for all three public names |
| `bridge.js` | event bridge, single dependency-free Node script |
| `routes.json` | CrowdRelay event name to n8n workflow mapping |

Installed at `${EDGE_ROOT}` (`/opt/virya-n8n` in production). Secrets and
`config.json` are host state and are not tracked here. This repository is
public, so the staff Basic Auth hash is injected as
`CONTROL_PLANE_STAFF_PASSWORD_HASH` rather than written into the `Caddyfile`.

## Upstreams

- `signal-api.virya.music` to `crowdrelay-api:8080` over `CROWDRELAY_DOCKER_NETWORK`
- `control.virya.music` to `control-plane:8090` over `virya-edge`
- `n8n.virya.music` to `VIRYA_N8N_UPSTREAM`, the n8n stack on virya-home, over WireGuard `wg0`

n8n stays on virya-home, which has no public address, so that one WireGuard hop
is the only remaining cross-host link.

## Run

```bash
export CROWDRELAY_DOCKER_NETWORK=crowdrelay-shared
export VIRYA_EDGE_PUBLIC_HOST=n8n.virya.music
export EDGE_ROOT=/opt/virya-n8n
export VIRYA_N8N_UPSTREAM=<virya-home wireguard address>:5678
export CONTROL_PLANE_STAFF_PASSWORD_HASH="$(sudo sed -n 's/.*staff //p' /etc/crowdrelay/edge-staff-hash)"
docker network create virya-edge 2>/dev/null || true
docker compose -f ops/edge/compose.edge.yaml up -d
```

## History

Before 2026-08-22 this ran on virya-oracle from `/opt/virya-n8n/docker-compose.yml`,
a file that had been deleted from the host while the containers kept running.
Nothing could recreate the stack and nothing recorded its wiring. The bridge was
also attached only to the n8n network, where the API could not reach it, so it
answered health checks and no real traffic; on this network it is reachable
again.
