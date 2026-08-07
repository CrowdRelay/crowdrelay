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
* canonical merch product catalog, variant-level inventory, stocktakes, staff READY activation, and Stripe order reservations;
* reward campaigns with reserved merch, weighted winner selection, and fulfillment tracking;
* short-lived, revocable event attendance QR codes;
* HMAC-signed webhooks, retries, idempotency, replay protection, and n8n integrations;
* role-scoped admin, staff, service, and commerce API namespaces with separate bearer credentials;
* optional Sigstore Rekor transparency-log anchoring for draw and audit receipts;
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
| Commerce         | staff/admin inventory + reward campaigns and `POST /v1/commerce/coupons/redeem`                                                                  |
| Synesthesia      | `POST /v1/public/synesthesia/runs`, ordered room completion, album completion and five-CD draw entry                                             |

Catalog, event and ticket-offer reads under `/public/*` are anonymous. Fan-specific `/me/*` routes use the private fan session, while ticket-order status and wallet routes use the per-order checkout bearer token. `/admin/*`, `/staff/*`, and service-only `/commerce/*` plus `/internal/*` routes are isolated authorization boundaries. Admin and staff credentials cannot call service routes, while service credentials cannot call operator routes. Exact schemas and error codes are documented in the OpenAPI contract. Architecture, reliability, inventory and external-proof operations are documented under `docs/`.

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

## Synesthesia eligibility plane

Migration `0030_synesthesia_ecosystem.sql` adds an isolated run/completion ledger. A completed run may create one draw entry per normalized e-mail for campaign `virya-synesthesia-album-v1`. This endpoint does not change `fan_consents`, enqueue mail, collect shipping PII or award referral/check-in weight. `synesthesia_completion` reward campaigns are server-locked to five winners, one physical item each and one equal entry per candidate; normal inventory reservation and Proof-of-Fair code is reused.

## Optional external proofs

CrowdRelay can create public SHA-256 draw receipts and Merkle commitments for
append-only audit records, then optionally publish signed commitments to the
Sigstore Rekor transparency log. PostgreSQL remains authoritative and Rekor
availability never enters a critical path. See `docs/EXTERNAL_PROOFS.md`.

## Engineering documentation

Architecture, ecosystem boundaries and reliability: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md), [`docs/VIRYA_ECOSYSTEM.md`](docs/VIRYA_ECOSYSTEM.md) and [`docs/RELIABILITY.md`](docs/RELIABILITY.md).
