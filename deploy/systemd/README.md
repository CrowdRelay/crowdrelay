# Production smoke timer

The recurring 15-minute smoke check runs on the server instead of consuming GitHub Actions minutes. GitHub keeps a manual `Production smoke` workflow for operator/post-deploy verification.

Install the service/timer under `/etc/systemd/system`, deploy this repository/package at `/srv/crowdrelay`, and define `CROWDRELAY_BASE_URL`, `VIRYA_BASE_URL`, optional `SYNESTHESIA_BASE_URL`, and optional `N8N_INGRESS_URL` in `/etc/virya/production-smoke.env`. Enable with `systemctl enable --now virya-production-smoke.timer`.

## Optional alerts

Set `ALERT_WEBHOOK_URL` in `/etc/virya/production-smoke.env` to preserve failure notifications after moving the 15-minute schedule out of GitHub Actions. Repeated failures are rate-limited to one alert per hour by default (`ALERT_COOLDOWN_SECONDS`), and the probe sends one recovery message when service returns. The timer keeps its state in `/var/lib/virya-production-smoke`.
