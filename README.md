# CrowdRelay

**Rust / Tokio / Axum / SQLx / PostgreSQL backend and deterministic growth plane for artists, events and communities. Stable release: 1.0.0.**

CrowdRelay owns the durable business state behind a working artist: fans and their consent, tickets, admission, merch, referrals, and every outward action the operation takes. It turns campaign traffic into one auditable flow — `smart link → signup → confirmation → referrals → event interest → reward / draw → check-in / admission` — and, through ViryaOS Autopilot, decides and runs the growth work around that flow under explicit, operator-set authority limits.

Email, n8n, Stripe, Calendar, Bandsintown and LLM-assisted copy adaptation are execution adapters. They deliver; they never hold business truth, choose recipients, invent offers or widen authority.

The interesting part is not the endpoint list. It is that **a decision, the evidence it was made on, the action it produced and the claim that action can support are four separate records**, so an autonomous system cannot quietly promote a coincidence into a result.

## Engineering snapshot

- **Consistency first:** multi-row business invariants, idempotency results and outbox intents are committed transactionally.
- **Asynchronous delivery:** public requests never wait for email, n8n or other providers; workers deliver from a transactional outbox with leases, bounded retries and dead-state inspection.
- **Explicit delivery semantics:** external delivery is at-least-once, so event/operation identity is durable and consumers must deduplicate.
- **Hard authorization boundaries:** `/public`, `/me`, `/admin`, `/staff`, `/commerce` and `/internal` are separate capabilities rather than one general bearer credential.
- **Bounded autonomy:** every autonomous action carries an action class, an operator ceiling per class, a weekly volume envelope and a per-contact cooldown. A new action kind does not compile until somebody has decided what it costs.
- **Evidence over assertion:** a measurement that cannot be made is stored and returned as `insufficient` with a reason. Nothing interpolates a missing point or reports a ratio against a baseline too flat to carry one.
- **Measured scaling boundary:** the API is stateless apart from bounded process-local read caches; workers coordinate through PostgreSQL leases. A separate broker or partitioning is deferred until production measurements justify the extra system.
- **Release identity:** production deploys are tied to an exact Git/OCI revision and verified with readiness and end-to-end management checks.
- **Cross-repo compatibility:** OpenAPI and executable ecosystem contracts guard consumers such as Virya Signal, virya.music and Synesthesia against backend drift.

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the consistency model and deliberate trade-offs, and [`docs/STABLE_CONTRACT.md`](docs/STABLE_CONTRACT.md) for the compatibility policy.

## Status

1.0.0, in production, 89 sequential migrations. The transactional core — fans, events, ticketing, admission, commerce, referrals, outbox delivery — is complete and carries live traffic.

The growth plane is the part still being built: 14 of 20 phases, with the reasoning behind each one, what was deliberately not built and why, tracked in [`docs/GROWTH_OS_PLAN.md`](docs/GROWTH_OS_PLAN.md). Every autopilot context is provisioned disabled and at `observe`; turning one on is a row update an operator makes deliberately.

## Features

- campaign links, attribution and asynchronous click statistics;
- double-opt-in fan signup, private fan sessions, consent and unsubscribe;
- first-party audience segments, tags, communication intents and funnel analytics;
- referrals, thresholds, coupons and physical reward fulfillment;
- city demand and event-interest capture;
- event catalog, reminders and Bandsintown synchronization;
- weighted draws with auditable snapshots and sampling without replacement;
- admission pools, rotating QR credentials and atomic venue redemption;
- ticket inventory, durable Stripe Checkout holds, refunds and paid-pass issuance;
- merch inventory, variants, stocktakes and Stripe order reservations;
- HMAC-signed webhooks, replay protection, idempotency and n8n execution adapters;
- optional Sigstore Rekor anchoring for audit/draw receipts;
- health, readiness, metrics, structured logging, migrations and graceful shutdown.

The exact request/response/auth schemas live in [`openapi/openapi.yaml`](openapi/openapi.yaml), which is the supported cross-repository integration contract. Internal Rust modules and persistence remain implementation details.

## ViryaOS Autopilot

ViryaOS is an opt-in deterministic operations plane built on CrowdRelay. Business decisions remain explicit Rust policy: scoring functions, state machines, versioned authority rules and auditable intent. n8n, email, Calendar and LLM-assisted copy adaptation are execution adapters only; they do not choose recipients, invent offers or broaden authority.

The operator-facing programs are:

- Release Autopilot;
- Fan Growth Autopilot;
- Press Autopilot;
- Opportunity Autopilot;
- Commerce Autopilot;
- Patronage & Endorsement Autopilot;
- Funding Autopilot.

Authority is classified as **AUTO**, **BOUNDED AUTO** or **APPROVAL**. Consequential paid, contractual or otherwise irreversible actions remain approval-gated. `CROWDRELAY_AUTOPILOT_ENABLED` is the hard kill switch for autonomous evaluation/execution.

Attendance growth and Beacon outreach are relationship- and policy-driven rather than generic blast messaging. The operational model is documented in [`docs/ATTENDANCE_GROWTH_PLAYBOOK.md`](docs/ATTENDANCE_GROWTH_PLAYBOOK.md).

### Current direction: from detector to agent

Every bounded context above answers one question per cycle and forgets. That shape suits a detector and not a campaign, so the plane is being extended with three things that let it act rather than only notice.

- **Bounded authority.** Every action carries an action class — first-party reversible, owned audience, third party or paid. An operator sets a ceiling per class, and a weekly volume envelope plus a per-contact cooldown bound blast radius independently of that ceiling.
- **Plays.** A play is a durable multi-step campaign anchored to a fact such as a show: ordered steps, each with its own action class and its own window derived from the anchor. A gated step waiting on a human never blocks the step behind it, and a step past its window is settled as skipped with its reason rather than delivered late or silently dropped. Steps execute through the existing outbox; there is no second scheduler.
- **Honest measurement.** The unit of measurement is the play, against a baseline frozen when it started. Two claims are kept separate and never merged: first-party rows that join an outcome to the action are reported as attribution, and a follower or tracker series moving over the play's window is reported as correlational. Where a join key, a baseline or an audience is missing, the API returns `evidence: insufficient` with the reason.

## Tech stack

Rust 1.97 (edition 2024), Tokio, Axum 0.8, SQLx 0.8 against PostgreSQL 18, `time`, `uuid` v7, `tracing` with a JSON subscriber, rustls only. No compile-time SQL macros, so the workspace builds and lints without a database.

| Crate | Responsibility |
| --- | --- |
| `crowdrelay-domain` | identifiers, events, value objects and deterministic policy |
| `crowdrelay-application` | use cases and repository/provider ports |
| `crowdrelay-infra` | PostgreSQL repositories, caches, provider adapters and observability |
| `crowdrelay-api` | HTTP/auth boundaries, validation and response contracts |
| `crowdrelay-worker` | outbox delivery, reminders, retention, synchronization and draws |

The intended dependency direction is domain → application → infrastructure/adapters, with transport and provider details kept outside business policy.

### Main command path

```text
HTTP command
  -> auth + bounded validation
  -> application use case
  -> PostgreSQL transaction
       business rows
       idempotency result
       outbox event
  -> response
  -> worker lease
  -> signed provider delivery
  -> retry / delivered / dead
```

PostgreSQL is authoritative for fan state, consent, tickets, admission, commerce, accounting-related state and the outbox. External systems never participate in the public transaction commit.

## API groups

| Group | Examples |
| --- | --- |
| Health | `/v1/health/live`, `/v1/health/ready` |
| Fans | signup, confirm, unsubscribe, referral/session reads |
| Events | public catalog, interest, calendar/share/listen/ticket actions |
| Admission | admin issue/revoke, fan wallet/QR, staff redemption |
| Ticketing | public reservation/status, admin inventory, internal reconciliation |
| Operations | queue/dead-item inspection, attempts, audited retry |
| Audience | Fan 360, segments, communication intents, analytics |
| Commerce | inventory, reward campaigns and coupon redemption |
| Synesthesia | run ledger, completion, leaderboard publication and CD draw entry |
| Autopilot | opportunities, Beacons, policy, approvals, plays and operator read models |

Public catalog reads are anonymous. Fan-specific `/me/*` routes use private fan sessions; ticket-order/wallet routes use per-order capability tokens. Admin, staff, service and internal namespaces are intentionally not interchangeable.

## Local development

```sh
cp .env.example .env
docker compose up --build -d
make setup
make check
```

`make check` is the canonical source gate for formatting, linting, tests and repository contracts.

## Production

```sh
cp .crowdrelay.local.sh.example .crowdrelay.local.sh
./crowdrelayctl init
./crowdrelayctl pin <FULL_COMMIT_SHA>
./crowdrelayctl doctor
./crowdrelayctl deploy
```

Production deployment is SHA-pinned. `.crowdrelay.local.sh`, environment files, bootstrap data and secrets are ignored by Git. The reverse proxy joins the application Docker network and routes to `crowdrelay-api:8080`; examples live under `deploy/reverse-proxy/`.

If release-ledger reporting is configured, deploy reporting is deliberately fail-open: telemetry cannot turn an otherwise healthy exact deploy into a failed release.

## Synesthesia eligibility plane

Synesthesia uses an isolated run/completion ledger. A valid completed run may create one entry in campaign `virya-synesthesia-album-v1`; this does **not** change marketing consent, collect shipping PII or add referral/check-in weight. The reward campaign is server-locked to five winners, one physical item each and equal candidate weight.

## Documentation

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — architecture, consistency and scaling trade-offs
- [`docs/STABLE_CONTRACT.md`](docs/STABLE_CONTRACT.md) — supported integration boundary
- [`docs/GROWTH_OS_PLAN.md`](docs/GROWTH_OS_PLAN.md) — autonomous growth plane: phases, decisions and what is deliberately not built yet
- [`docs/ATTENDANCE_GROWTH_PLAYBOOK.md`](docs/ATTENDANCE_GROWTH_PLAYBOOK.md) — deterministic attendance-growth operating model
- [`docs/EXPERIMENTATION_PLAYBOOK.md`](docs/EXPERIMENTATION_PLAYBOOK.md) — bounded experimentation
- [`docs/operations/`](docs/operations/) — production/operator notes

## License

Apache-2.0. See [`LICENSE`](LICENSE).
