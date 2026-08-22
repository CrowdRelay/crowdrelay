# Server timers

Two independent units live here. Both name `/opt/crowdrelay`, which is where
production is installed; check `WorkingDirectory`/`ExecStart` against the real
install before enabling one anywhere else.

## Control Plane heartbeat timer

The Control Plane marks a tenant `stale` after
`CONTROL_PLANE_RUNTIME_STALE_AFTER_SECONDS` (180s by default), so reporting only
at deploy time leaves the panel correct for three minutes and wrong afterwards.
`virya-crowdrelay-heartbeat.timer` runs `crowdrelayctl heartbeat` every 60s to
keep it current.

It is inert until `CROWDRELAY_CONTROL_PLANE_BASE_URL`,
`CROWDRELAY_CONTROL_PLANE_TELEMETRY_TOKEN` and
`CROWDRELAY_CONTROL_PLANE_TENANT_SLUG` are set in `.crowdrelay.local.sh`; without
them the command returns success and reports nothing. Point the base URL at
whatever address the host can reach the Control Plane on directly — the public
origin sits behind edge Basic Auth that also rewrites `Authorization`, which
would strip the telemetry bearer.

Enable with `systemctl enable --now virya-crowdrelay-heartbeat.timer`.

## Production smoke timer

The recurring 15-minute smoke check runs on the server instead of consuming GitHub Actions minutes. GitHub keeps a manual `Production smoke` workflow for operator/post-deploy verification.

Install the service/timer under `/etc/systemd/system`, deploy this repository/package at `/opt/crowdrelay`, and define `CROWDRELAY_BASE_URL`, `VIRYA_BASE_URL`, optional `SYNESTHESIA_BASE_URL`, and optional `N8N_INGRESS_URL` in `/etc/virya/production-smoke.env`. Enable with `systemctl enable --now virya-production-smoke.timer`.

## Optional alerts

Set `ALERT_WEBHOOK_URL` in `/etc/virya/production-smoke.env` to preserve failure notifications after moving the 15-minute schedule out of GitHub Actions. Repeated failures are rate-limited to one alert per hour by default (`ALERT_COOLDOWN_SECONDS`), and the probe sends one recovery message when service returns. The timer keeps its state in `/var/lib/virya-production-smoke`.
