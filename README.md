# CrowdRelay

**A deterministic operations engine that runs a metal band's growth autonomously.**

CrowdRelay owns the durable business state behind audience growth and live operations: fans and consent, events, tickets, admission, merch, referrals, venue relationships, community outreach and every action taken around them. It decides what to do within explicit authority limits, executes through external systems, measures what happened and feeds results into the next decision cycle.

The core idea: **business state and business decisions stay in CrowdRelay; external services only execute the work they are asked to do.** Email, n8n, Stripe, Calendar, Bandsintown and LLM-assisted copy are adapters, not sources of truth.

There is no AI agent in here — deliberately. The system is deterministic Rust, state machines, enums, persisted snapshots, explicit conditions, bounded authority, retries and failure handling. It finds opportunities, makes decisions, executes work, survives failures and knows when it's not allowed to act.

## What the brain does

The Autopilot evaluates twenty-one bounded contexts on every cycle:

| context | what it handles |
|---|---|
| **Ticket Yield** | sell-through, paid velocity, capacity moves under guardrails |
| **Fan Lifecycle** | deterministic communication steps per fan lifecycle stage |
| **Campaign Lifecycle** | event campaign phases with consent-gated messaging |
| **Merchandising** | stock coverage, reorder windows, bundle economics |
| **Merch Pricing** | price moves preserving margin floors |
| **Merch Bundle** | affinity-based bundle requests |
| **Booking Opportunity** | city demand scoring, verified outreach targets, cooldowns |
| **Outreach** | playlist curators, press, radio, creators, media patronage |
| **Content Supply** | release artifacts, content source verification |
| **Promotion Budget** | ROAS observation, budget bounds |
| **Experimentation** | traffic allocation with sufficient-evidence checks |
| **Show Operations** | task completion proven from system state vs human-required |
| **Release** | milestones, editorial pitch chase, calendar sync |
| **Live Opportunity** | gig economics, strategic value, negotiation terms ladder |
| **Funding** | funding package preparation and submission |
| **Beacon** | scene-partner discovery, local signal amplification, invite batches |
| **Show Growth** | free listing sweeps, audience capture, organic channel push |
| **Growth Metrics** | trend and anomaly detection across metric series |
| **Growth Debt** | neglected committed work: quiet relationships, missed milestones, missing assets |
| **Outreach Supply** | detects starved pipelines, requests discovery sweeps |
| **Plays** | multi-step campaigns with fan anchors and consent re-checks |

Every context passes through a shared funnel: confidence gate → authority level → class ceiling → envelope budget → deliverability halt. The stricter limit wins. Money and contracts stay behind approval in every posture.

## Authority model

| posture | what the agent does | what it never does |
|---|---|---|
| `grounded` | observes and rehearses everything (dry run) | touch anyone |
| `working` | first-party work runs alone; outward contact drafts for approval | send to fans or curators unattended |
| `full_send` | owned audience sends within limits; free pitching runs unattended | spend money, sign contracts |

One dial applies all twenty-one context levels, four class ceilings and the envelope switches atomically. Budgets are operator-tuned and survive posture flips. Per-context knobs (screening floors, lead windows, cooldowns) are editable without code changes.

## Safety

- Deliverability ramp: sending volume earns its ceiling from zero; bounce or complaint rates close it before damage
- Drawdown halts close ordering when losses breach profile limits
- Every action carries an idempotency key; retries are bounded and auditable
- Circuit breakers open on executor failure storms
- Kill switch stops all ordering without touching positions or data
- Blue/green deploys cannot let two workers jointly exceed a quota
- HMAC-signed webhooks with replay protection
- Runtime panics forbidden at compile time (`#![forbid(unsafe_code)]` + deny list)

## Measurement and learning

Executed actions settle against benchmarks after their horizon. Effects are labelled `improved`, `neutral` or `worsened` — never a raw score without context. Strategies whose record repeatedly worsens retire themselves with a stated reason. Attribution is honest: smart-link clicks are attribution; follower movement after a campaign is correlational, always labelled as such.

The daily brief breaks silence only for things that lie when quiet: halted ceilings, stale approvals, dead executors, pending withdrawals. Everything else waits for somebody to look at the panel.

## Features

- tracked smart links with channel/community/creative attribution;
- double-opt-in fan signup, private fan sessions, consent and unsubscribe;
- audience segments, tags, communication intents and funnel analytics;
- referrals, thresholds, coupons and reward fulfillment;
- city demand and market-intelligence capture with provenance and expiry;
- event catalog, reminders and Bandsintown synchronization;
- weighted draws with auditable snapshots and sampling without replacement;
- admission pools, rotating QR credentials and atomic venue redemption;
- ticket inventory, Stripe Checkout holds, refunds and paid-pass issuance;
- merch inventory, variants, stocktakes and Stripe order reservations;
- venue/promoter discovery with screened-on-write candidates;
- booking negotiation with computed cost floors and terms ladder;
- free-reach pitching waves with evidence packets and placement verification;
- beacon network: scene-partner discovery, local amplification, invite batches;
- community outreach packs assigned to social-skill team members;
- deliverability ramp and bounce/complaint halts;
- momentum-pullback and core-beta strategies with benchmark-settled outcomes;
- strategy standings that narrow allocation before retiring;
- daily operator brief with silence-default rules;
- transactional outbox delivery with bounded retries and dead-state inspection;
- optional Sigstore Rekor anchoring for audit and draw receipts;
- health, readiness, metrics, structured logging, migrations and graceful shutdown.

The supported HTTP integration contract is [`openapi/openapi.yaml`](openapi/openapi.yaml).

## Tech stack

Rust 1.98 (edition 2024), Tokio, Axum 0.8, SQLx 0.8 and PostgreSQL 19 (dev/CI run the 19 beta; GA flip tracked in `scripts/local/pg-beta-to-ga-upgrade.sh`).

| Crate | Responsibility |
| --- | --- |
| `crowdrelay-domain` | identifiers, events, value objects and deterministic policy |
| `crowdrelay-application` | use cases and repository/provider ports |
| `crowdrelay-infra` | PostgreSQL repositories, caches, provider adapters and observability |
| `crowdrelay-api` | HTTP/auth boundaries, validation and response contracts |
| `crowdrelay-worker` | outbox delivery, reminders, retention, synchronization and draws |

The architecture keeps business policy independent from transport and provider details, with PostgreSQL authoritative for durable business state.

## Getting started

```bash
cp .env.example .env
just setup          # start Postgres + apply migrations + bootstrap workspace
just check          # fmt + clippy -D warnings + tests
just ci             # everything CI runs
```

## Deployment

```bash
just deploy         # waits for CI, pulls validated images, ships to virya-home
```

See [`docs/AUTONOMY_RUNBOOK.md`](docs/AUTONOMY_RUNBOOK.md) for the go-live checklist.

## License

Apache-2.0. See [`LICENSE`](LICENSE).
