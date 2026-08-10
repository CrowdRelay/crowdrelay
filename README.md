# CrowdRelay

## What It Does and Why It Exists

CrowdRelay is a self-hosted backend for artists, events, and communities. It turns campaign traffic into a measurable flow:

`smart link → fan signup → confirmation → referrals → event interest → reward / draw → check-in / admission`

The operator owns the data. Public requests never send emails or call external providers synchronously; all deliveries are processed through a transactional outbox.

## Features

* campaigns, smart links, redirect attribution, and asynchronous click statistics;
* fan signup, consent management, double opt-in, unsubscribe, and private sessions;
* first-party Fan 360 audience intelligence, reusable segments, operator tags, communication intents, and currency-safe funnel analytics;
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
* role-scoped admin, staff, service, and commerce API namespaces; staff pairing issues short-lived one-time codes and revocable per-device bearer sessions, while the legacy static staff bearer remains compatibility-only and is metered for retirement;
* optional Sigstore Rekor transparency-log anchoring for draw and audit receipts;
* PostgreSQL, migrations, health checks, metrics, structured logging, and graceful shutdown;
* an OpenAPI 3.1 contract used as the canonical cross-repository API boundary.

## ViryaOS Autopilot

ViryaOS Autopilot is an opt-in, deterministic operations plane built on CrowdRelay. Its core loop is:

`first-party + market facts → bounded-context decision → autonomy policy → durable action → measured outcome`

Business rules live in small Rust domain modules rather than n8n or provider workflows. Current bounded capabilities cover ticket yield, fan and campaign lifecycle, merch stock/pricing/bundles, booking opportunities and outreach, content supply, promotion budgets, experiments, and show operations. External market signals are typed, confidence-scored, expiring inputs; they cannot bypass first-party evidence or policy limits.

The global `CROWDRELAY_AUTOPILOT_ENABLED` switch is off by default, and each capability has its own versioned authority, confidence threshold and 24-hour action quota. Durable action jobs are idempotent, bounded-retry, crash-recoverable and auditable. Financial changes use explicit operator-owned guardrails, while delayed outcome measurements keep execution evidence separate from effectiveness evidence. n8n and provider integrations remain execution adapters: they receive typed intents and report facts, but do not own business decisions.

PostgreSQL 18 is the persistence baseline. The local stack uses its asynchronous I/O subsystem conservatively and exposes the active server/AIO settings through operations telemetry so tuning can be based on the production host rather than assumed defaults.

## Integration and Usage

### Entry Points

| Entry Point               | Purpose                                                                                 |
| ------------------------- | --------------------------------------------------------------------------------------- |
| `crowdrelay-api`          | HTTP API exposed under the `/v1` prefix                                                 |
| `crowdrelay-worker run`   | outbox processing, reminders, retention, prize draws, and event synchronization         |
| `crowdrelay-worker setup` | migrations and idempotent workspace bootstrap                                           |
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
| Check-in         | `POST /v1/events/{slug}/check-in`                                                            |
| Admin QR         | `GET /v1/admin/event-qr/overview`, `GET/POST /v1/admin/event-qr/campaigns`, `POST .../{id}/revoke`                                                |
| Admission Passes | admin issue/revoke, fan claim/status/QR, and staff redemption under `/v1/admin/admission`, `/v1/passes`, `/v1/me/pass`, and `/v1/staff/admission` |
| Ticketing        | public sale/reservation/status, admin configuration/overview, and authenticated Stripe reconciliation under `/v1/public`, `/v1/admin`, and `/v1/internal` |
| Operations       | admin queue summary, dead-item inspection, delivery attempt history, and audited manual retry under `/v1/admin/ops`                                    |
| Audience         | admin-only Fan 360, segments, communication intents and analytics under `/v1/admin/audience`, `/v1/admin/communications` and `/v1/admin/analytics` |
| Commerce         | staff/admin inventory + reward campaigns and `POST /v1/commerce/coupons/redeem`                                                                  |
| Synesthesia      | `POST /v1/public/synesthesia/runs`, ordered room completion, album completion and five-CD draw entry                                             |

Catalog, event and ticket-offer reads under `/public/*` are anonymous. Fan-specific `/me/*` routes use the private fan session, while ticket-order status and wallet routes use the per-order checkout bearer token. `/admin/*`, `/staff/*`, and service-only `/commerce/*` plus `/internal/*` routes are isolated authorization boundaries. Admin credentials and staff device sessions cannot call service routes, while service credentials cannot call operator routes. The legacy static staff bearer is accepted only as a measured compatibility fallback until production telemetry confirms zero use. Exact schemas and error codes are documented in the OpenAPI contract. The maintained architecture documentation lives under `docs/`.

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

Synesthesia uses an isolated run/completion ledger. A completed run may create one draw entry per normalized e-mail for campaign `virya-synesthesia-album-v1`. This endpoint does not change `fan_consents`, enqueue mail, collect shipping PII or award referral/check-in weight. `synesthesia_completion` reward campaigns are server-locked to five winners, one physical item each and one equal entry per candidate; normal inventory reservation and Proof-of-Fair code is reused.

## Engineering documentation

Architecture: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).
