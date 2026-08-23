# CrowdRelay

**CrowdRelay is a deterministic backend and operations engine for artists, events and communities.**

It owns the durable business state behind audience growth and live operations: fans and consent, events, tickets, admission, merch, referrals and the actions taken around them.

The core idea is simple: **business state and business decisions stay in CrowdRelay; external services only execute the work they are asked to do.** Email, n8n, Stripe, Calendar, Bandsintown and LLM-assisted copy are adapters, not sources of business truth.

CrowdRelay is designed for reliable automation: writes are transactional, external delivery is asynchronous and at-least-once, retries are bounded and auditable, and consequential automation stays within explicit authority limits.

## Features

- campaign links, attribution and click statistics;
- double-opt-in fan signup, private fan sessions, consent and unsubscribe;
- audience segments, tags, communication intents and funnel analytics;
- referrals, thresholds, coupons and reward fulfillment;
- city demand and event-interest capture;
- event catalog, reminders and Bandsintown synchronization;
- weighted draws with auditable snapshots and sampling without replacement;
- admission pools, rotating QR credentials and atomic venue redemption;
- ticket inventory, Stripe Checkout holds, refunds and paid-pass issuance;
- merch inventory, variants, stocktakes and Stripe order reservations;
- HMAC-signed webhooks, replay protection and idempotency;
- transactional outbox delivery with bounded retries and dead-state inspection;
- deterministic ViryaOS automation with bounded authority and approval gates;
- optional Sigstore Rekor anchoring for audit and draw receipts;
- health, readiness, metrics, structured logging, migrations and graceful shutdown.

The supported HTTP integration contract is [`openapi/openapi.yaml`](openapi/openapi.yaml).

## Tech stack

Rust 1.97 (edition 2024), Tokio, Axum 0.8, SQLx 0.8 and PostgreSQL 18.

| Crate | Responsibility |
| --- | --- |
| `crowdrelay-domain` | identifiers, events, value objects and deterministic policy |
| `crowdrelay-application` | use cases and repository/provider ports |
| `crowdrelay-infra` | PostgreSQL repositories, caches, provider adapters and observability |
| `crowdrelay-api` | HTTP/auth boundaries, validation and response contracts |
| `crowdrelay-worker` | outbox delivery, reminders, retention, synchronization and draws |

The architecture keeps business policy independent from transport and provider details, with PostgreSQL authoritative for durable business state.

## License

Apache-2.0. See [`LICENSE`](LICENSE).
