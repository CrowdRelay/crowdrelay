# CrowdRelay

## What It Does and Why It Exists

CrowdRelay is a self-hosted backend for artists, events, and communities. It turns campaign traffic into a measurable flow:

`smart link → fan signup → confirmation → referrals → event interest → reward / draw → check-in / admission`

The operator owns the data. Public requests never send emails or call external providers synchronously; all deliveries are processed through a transactional outbox.

## Features

* campaigns, smart links, redirect attribution, and asynchronous click statistics;
* fan signup, consent management, double opt-in, unsubscribe, and private sessions;
* referral links, reward thresholds, coupons, and physical reward fulfillment;
* city-level interest aggregation;
* event catalog, fan actions, reminders, and Bandsintown synchronization;
* weighted prize draws with auditable weight snapshots and sampling without replacement;
* admission pass pools, claims, rotating QR codes, and atomic redemption at the venue;
* first-party ticket inventory with explicit sold, held, and available counters, durable Stripe Checkout holds, VAT-inclusive pricing, refunds, and paid pass issuance;
* short-lived, revocable event attendance QR codes;
* HMAC-signed webhooks, retries, idempotency, replay protection, and n8n integrations;
* role-scoped admin, staff, and service API namespaces with separate bearer credentials;
* PostgreSQL, migrations, health checks, metrics, structured logging, and graceful shutdown;
* a dependency-free TypeScript client and an OpenAPI 3.1 contract.

## Integration and Usage

### Entry Points

| Entry Point               | Purpose                                                                                 |
| ------------------------- | --------------------------------------------------------------------------------------- |
| `crowdrelay-api`          | HTTP API exposed under the `/v1` prefix                                                 |
| `crowdrelay-worker run`   | outbox processing, reminders, retention, prize draws, and event synchronization         |
| `crowdrelay-worker setup` | migrations and idempotent workspace bootstrap                                           |
| `packages/crowdrelay-js`  | TypeScript client for frontend and backend applications                                 |
| `openapi/openapi.yaml`    | complete request, response, and authentication contract                                 |
| `crowdrelayctl`           | local deployment, SHA pinning, verification, logs, backup hooks, and SSH-based shipping |

### API

| Group            | Endpoints                                                                                                                                         |
| ---------------- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| Health           | `GET /v1/health/live`, `GET /v1/health/ready`                                                                                                     |
| Campaigns        | `GET /v1/go/{slug}`, `GET /v1/r/{code}`                                                                                                           |
| Fans             | `POST /v1/fans`, `/fans/confirm`, `/fans/unsubscribe`, `GET /v1/me/referral`                                                                      |
| Cities           | `GET /v1/public/cities`                                                                                                                           |
| Events           | `GET /v1/public/events`, `/public/events/{slug}`, and the `view`, `ticket`, `listen`, `calendar.ics`, and `share` actions                         |
| Interest         | `POST /v1/events/{slug}/interest`, `GET /v1/me/events`                                                                                            |
| Check-in         | `POST /v1/events/{slug}/check-in`                                                                                                                 |
| Admin QR         | `GET /v1/admin/event-qr/overview`, `GET/POST /v1/admin/event-qr/campaigns`, `POST .../{id}/revoke`                                                |
| Admission Passes | admin issue/revoke, fan claim/status/QR, and staff redemption under `/v1/admin/admission`, `/v1/passes`, `/v1/me/pass`, and `/v1/staff/admission` |
| Ticketing        | public sale/reservation/status, admin configuration/overview, and authenticated Stripe reconciliation under `/v1/public`, `/v1/admin`, and `/v1/internal` |
| Operations       | admin queue summary, dead-item inspection, delivery attempt history, and audited manual retry under `/v1/admin/ops`                                    |
| Commerce         | `POST /v1/commerce/coupons/redeem`                                                                                                                |

Catalog, event and ticket-offer reads under `/public/*` are anonymous. Fan-specific `/me/*` routes use the private fan session, while ticket-order status and wallet routes use the per-order checkout bearer token. `/admin/*`, `/staff/*`, and service-only `/commerce/*` plus `/internal/*` routes are isolated authorization boundaries. Admin and staff credentials cannot call service routes, while service credentials cannot call operator routes. Exact schemas and error codes are documented in the OpenAPI contract. First-party ticketing invariants and deployment order are documented in [`docs/TICKETING.md`](docs/TICKETING.md). Operations endpoints and their staged rollout are documented in [`docs/OPS_CONTROL_PLANE.md`](docs/OPS_CONTROL_PLANE.md). The cross-product event, Signal, AREA, ticket, accounting and n8n boundaries are summarized in [`docs/VIRYA_ECOSYSTEM.md`](docs/VIRYA_ECOSYSTEM.md).

### Local Development

```sh
cp .env.example .env
docker compose up --build -d
make setup
make check
```

### Production

```sh
cp .crowdrelay.local.sh.example .crowdrelay.local.sh
./crowdrelayctl init
./crowdrelayctl pin <FULL_COMMIT_SHA>
./crowdrelayctl doctor
./crowdrelayctl deploy
```

`.crowdrelay.local.sh`, the production environment file, bootstrap data, and webhook secrets are ignored by Git. The local configuration file stores the pinned SHA, paths, SSH target, and optional `crowdrelay_before_deploy`, `crowdrelay_after_deploy`, `crowdrelay_after_verify`, `crowdrelay_backup`, and `crowdrelay_notify` functions.

The reverse proxy must join the same Docker network and route traffic to `crowdrelay-api:8080`. Minimal Caddy and Nginx examples are available under `deploy/reverse-proxy/`.

## License

Apache-2.0. See `LICENSE`.

Mobile operator API: [`docs/MOBILE_APP.md`](docs/MOBILE_APP.md).


## Ecosystem max control plane

The private control plane includes auditable feature flags, reconciliation,
show checklists, a signed offline gate snapshot, correlation propagation and
restore/load/contract tooling. See `docs/ECOSYSTEM_MAX.md`.


## Optional external proofs

CrowdRelay can create public SHA-256 draw receipts and Merkle commitments for
append-only audit records, then optionally publish signed commitments to the
Sigstore Rekor transparency log. PostgreSQL remains authoritative and Rekor
availability never enters a critical path. See `docs/EXTERNAL_PROOFS.md`.

## Engineering documentation

Architecture and reliability: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) and [`docs/RELIABILITY.md`](docs/RELIABILITY.md).
