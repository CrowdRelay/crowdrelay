# VIRYA manager configuration

VIRYA keeps operator-editable manager preferences outside Git, while CrowdRelay remains the durable runtime source of truth.

## Google Sheet → CrowdRelay

A private Google Sheet may hold values such as annual show target/stretch ceiling, priority markets and team contact addresses. A private n8n workflow reads those cells and first calls `GET /v1/admin/autopilot/manager-config/booking-policy`. It then validates/normalizes the Sheet values and submits `POST /v1/admin/autopilot/manager-config/booking-policy` with the returned `version` as `expected_version`. CrowdRelay rejects a stale write, validates the policy and stores the last valid version in `viryaos_manager_config`; a temporary Drive or n8n outage therefore does not change booking behaviour.

Recommended operator fields:

| Key | Example / meaning |
| --- | --- |
| `annual_target` | normal number of shows to plan for in a calendar year |
| `annual_stretch` | hard stretch ceiling used only for exceptional opportunities |
| `stretch_minimum_score_basis_points` | minimum score needed once the normal target is reached |
| `prefer_weekend_one_shots` | prefer practical Fri/Sat/Sun one-offs over continuous touring |
| `priority_markets` | ordered markets such as `PL`, `DE-EAST`, `CZ`, `SK` |
| `far_shot_minimum_score_basis_points` | stronger threshold for distant travel |

The current VIRYA defaults are 15 normal / 20 stretch, weekend one-shots preferred, with Poland first, then eastern Germany and Czechia/Slovakia. These are defaults, not hard-coded commitments: the persisted manager policy wins.

## Team contacts

Real team e-mail addresses are deployment secrets and are intentionally absent from source control. The worker reads only these variable names:

- `VIRYA_TEAM_WOJTEK_EMAIL`
- `VIRYA_TEAM_LUBEK_EMAIL`
- `VIRYA_TEAM_KUBA_EMAIL`
- `VIRYA_TEAM_MARCIN_EMAIL`
- `VIRYA_TEAM_MAREK_EMAIL`

When `CROWDRELAY_ENV=production` and `CROWDRELAY_AUTOPILOT_ENABLED=true`, all five contact secrets are required. Production therefore fails closed instead of accepting approvals that cannot notify their assigned owner.

A private Sheet can be the human-readable backup, but secret injection remains the deployment boundary. The Sheet is not queried on every assignment.

## Assignment semantics

CrowdRelay routes human handoffs by capability first and fair load second. It deliberately avoids pure random assignment. The notification email contains what needs doing, why it matters, the deadline and a link to the canonical staff action; approval status and task state remain in CrowdRelay.

## Sync discipline

Treat the Sheet as an operator editing surface, not a runtime dependency. A sync should record the Sheet revision in `source_revision`, use `source=google_sheets`, and stop on a version conflict instead of overwriting a newer operator change. Read-after-write is recommended for an audit log, but assignments and booking evaluation always read CrowdRelay's persisted last-valid policy.
