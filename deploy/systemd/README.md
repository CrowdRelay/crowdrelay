# Server timers

Three independent timers live here, plus the CI runner slice. All name `/opt/crowdrelay`, which is where
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

Install the service/timer under `/etc/systemd/system`, deploy this repository/package at `/opt/crowdrelay`, and define `CROWDRELAY_BASE_URL`, `VIRYA_BASE_URL`, optional `SYNESTHESIA_BASE_URL`, and optional `N8N_INGRESS_URL` in `/etc/virya/production-smoke.env`. Enable with `systemctl enable --now crowdrelay-production-smoke.timer`.

## Backup timer

`virya-crowdrelay-backup.timer` runs `ops/backup/backup.sh` nightly at 03:17
(local time). Each run dumps the authoritative PostgreSQL container in custom
format, writes a SHA-256 sidecar, prunes past `CROWDRELAY_BACKUP_RETAIN_DAYS`
(default 14), then verifies the fresh dump through the isolated restore
rehearsal. Any failure fails the unit and posts one rate-limited alert to
`ALERT_WEBHOOK_URL`. Configure paths, retention, and the alert webhook in
`/etc/virya/backup.env`; copy dumps off-host after each run. Targets,
restore steps, and the WAL-archiving upgrade path live in
`docs/DISASTER_RECOVERY.md`. Enable with
`systemctl enable --now virya-crowdrelay-backup.timer`.

## Optional alerts

Set `ALERT_WEBHOOK_URL` in `/etc/virya/production-smoke.env` to preserve failure notifications after moving the 15-minute schedule out of GitHub Actions. Repeated failures are rate-limited to one alert per hour by default (`ALERT_COOLDOWN_SECONDS`), and the probe sends one recovery message when service returns. The timer keeps its state in `/var/lib/crowdrelay-production-smoke`.

## CI runner slice

The self-hosted GitHub Actions runners (five `actions.runner.*.service` units)
run on the production host. During a Rust CI build host CPU pressure reached
`some avg60=89%` on 2 vCPUs and single-row database updates took 1-1.5 s
(2026-09-27/28). `ci-runners.slice` puts every runner below production: CPU and
IO weight 20 against docker's default 100, `MemoryHigh=4G`, `Nice=10`. Weights,
not quotas, so CI still gets the whole box when production is idle.

Install on the host between CI jobs (a changed runner restarts, interrupting a
job in flight):

    bash scripts/apply-ci-runner-limits.sh --dry-run
    bash scripts/apply-ci-runner-limits.sh

It is idempotent and prints each runner's slice afterwards. Moving the runners
off the production host is still the better fix; this is the cheap one.

