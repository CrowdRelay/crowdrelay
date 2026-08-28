# CrowdRelay

**A deterministic growth-operations platform for labels, artist rosters and festivals.**

CrowdRelay owns the durable business state behind audience growth and live operations: fans and consent, events, tickets, admission, merch, referrals, venue relationships, community outreach and every action taken around them. It decides what to do within explicit authority limits, executes through external systems, measures what happened and feeds results into the next decision cycle.

It is built to run **a whole roster from one seat**: each artist is a workspace inside a label organization, and the portfolio layer lets a roster's audiences amplify each other through explicit, revocable, capped consent edges — the one capability that only exists when one platform holds every artist's fan graph. The first tenant running it in production is Virya; everything multi-tenant is designed so onboarding another act or festival is workspace provisioning, not a fork.

The core idea: **business state and business decisions stay in CrowdRelay; external services only execute the work they are asked to do.** Email, n8n, Stripe, Calendar, Bandsintown and LLM-assisted copy are adapters, not sources of truth.

There is no LLM making decisions in here — deliberately. The brain is deterministic Rust, state machines, enums, persisted snapshots, explicit conditions, bounded authority, retries and failure handling. It finds opportunities, makes decisions, executes work, survives failures and knows when it's not allowed to act. The brain dispatches LLM workers (press pitches, social posts, community engagement drafts) via the separately deployed `crowdrelay-agents` service — the LLMs produce drafts, the brain decides what to do with them.

## What the brain does

The Autopilot evaluates twenty-two bounded contexts on every cycle:

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
| **Growth Intelligence** | dispatches LLM workers (reddit-scanner, press-pitch, social-post, community-engager, signal-inviter, growth-strategist) on cooldown-based cadences; feeds previous insights back into the next dispatch |
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

## Label Portfolio Mode

One organization, many artist workspaces, one operator view:

- roster-wide audience KPIs (active fans, 30-day growth, live amplification edges);
- consent edges between artists with purpose (`cross_promote`, `release_feature`, `event_crossbill`), monthly campaign caps and per-fan cooldowns;
- amplification campaigns that enqueue through the audience owner's own outbox — reach numbers for the beneficiary, no identities ever leave home;
- revocable edges with an approval paper trail; paused edges stop producing audience instantly.

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
- audience graph: prospecting map of communities (subreddits, Discords, forums, playlists) with each place's own promotion rules, evidence ledger and outreach pipeline;
- fanbases: addressable audience blocks with swappable origins (Meta lead ads, Bandsintown, Google, Reddit, CSV/HTTP pull), consent-safe ingestion and per-source attribution;
- booking negotiation with computed cost floors and terms ladder;
- free-reach pitching waves with evidence packets and placement verification;
- beacon network: scene-partner discovery, local amplification, invite batches;
- community outreach packs assigned to social-skill team members;
- growth intelligence loop: deterministic brain dispatches LLM workers (reddit-scanner, press-pitch, social-post, community-engager, signal-inviter, growth-strategist) on cooldown cadences, feeds insights back into the next dispatch, and maps worker outcomes into outreach targets and community posts;
- Reddit authenticated scraping: Playwright-based scraper in `crowdrelay-agents` logs into Reddit via Google OAuth, extracts session cookies, and serves them to the worker for authenticated JSON API access (bypasses Reddit's JS bot-detection challenge);
- deliverability ramp and bounce/complaint halts;
- momentum-pullback and core-beta strategies with benchmark-settled outcomes;
- strategy standings that narrow allocation before retiring;
- daily operator brief with silence-default rules;
- portfolio case-study export: one JSON with roster KPIs and every live amplification edge;
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
