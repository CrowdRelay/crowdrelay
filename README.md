# CrowdRelay

**Stable release: 1.0.0.** The OpenAPI 1.x document is the supported cross-team integration contract; internal Rust modules and persistence are implementation details.

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

ViryaOS Autopilot is an opt-in operations plane built on CrowdRelay. All business intelligence is deterministic Rust: explicit DDD decision services, scoring functions, state machines and versioned policies. There is no LLM or ML decision path. n8n, email, Calendar and other providers are execution adapters only; they receive typed intents and report facts, while CrowdRelay owns the decision and its audit trail.

The team-facing system is intentionally described as seven programs rather than the internal bounded contexts:

* **Release Autopilot** — release timeline, Calendar milestones, first-party fan campaigns, press/patronage/endorsement opportunity seeding and post-release sustain;
* **Fan Growth Autopilot** — consent-safe welcome, release follow-up, referral-oriented warm-up and dormant-fan reactivation with shared cooldowns;
* **Press Autopilot** — relationship-aware review/interview/radio/creator outreach with verified targets, relevance thresholds and bounded follow-up;
* **Opportunity Autopilot** — festival/showcase/review-contest/support-slot scoring and application; only free, non-exclusive, non-contractual, high-confidence applications may be sent automatically;
* **Commerce Autopilot** — ticket yield, bounded ticket-pool expansion, first-party campaign lifecycle, merchandising and deterministic experiments; it never performs per-fan price discrimination;
* **Patronage & Endorsement Autopilot** — media-patronage and gear-relationship outreach through the same verified relationship graph and cooldown rules as Press;
* **Funding Autopilot** — funding discovery facts, eligibility/economics evaluation, deadline Calendar intents and deterministic application-package preparation; final submission always requires approval.

Every action is governed by one of three authority levels: **AUTO** for reversible operational work, **BOUNDED AUTO** for actions inside explicit operator-owned limits, and **APPROVAL** for contractual, paid or otherwise consequential actions. The global `CROWDRELAY_AUTOPILOT_ENABLED` switch remains the hard kill switch, and every bounded context also has a versioned authority, confidence threshold and 24-hour action quota. Durable actions are idempotent, bounded-retry and auditable. Approval actions emit one provider-neutral notification event so the team is interrupted only when a human decision is actually required.

No Meta Ads executor is part of ViryaOS Autopilot. Promotion-budget telemetry may remain available for analysis, but paid advertising is not autonomously executed by this system.

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

If `CROWDRELAY_LEDGER_COMMERCE_API_KEY` and `CROWDRELAY_PUBLIC_BASE_URL` are configured locally, a verified deploy reports both `crowdrelay-api` and `crowdrelay-worker` to the ViryaOS release ledger. Reporting is intentionally fail-open and can never turn a healthy production deploy into a failed deploy.

The reverse proxy must join the same Docker network and route traffic to `crowdrelay-api:8080`. Minimal Caddy and Nginx examples are available under `deploy/reverse-proxy/`.

## License

Apache-2.0. See `LICENSE`.

## Synesthesia eligibility plane

Synesthesia uses an isolated run/completion ledger. A completed run may create one draw entry per normalized e-mail for campaign `virya-synesthesia-album-v1`. This endpoint does not change `fan_consents`, enqueue mail, collect shipping PII or award referral/check-in weight. `synesthesia_completion` reward campaigns are server-locked to five winners, one physical item each and one equal entry per candidate; normal inventory reservation and Proof-of-Fair code is reused.

## Engineering documentation

Architecture: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).
