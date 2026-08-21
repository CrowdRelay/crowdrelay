# CrowdRelay

**Rust / Tokio / Axum / SQLx / PostgreSQL. Stable release: 1.0.0.**

CrowdRelay is a self-hosted, PostgreSQL-authoritative backend for artists, events and communities. It turns campaign traffic into a durable flow:

`smart link → signup → confirmation → referrals → event interest → reward / draw → check-in / admission`

The OpenAPI 1.x document is the supported cross-repository integration contract. Internal Rust modules and persistence remain implementation details.

## Engineering snapshot

- **Consistency first:** multi-row business invariants, idempotency results and outbox intents are committed transactionally.
- **Asynchronous delivery:** public requests never wait for email, n8n or other providers; workers deliver from a transactional outbox with leases, bounded retries and dead-state inspection.
- **Explicit delivery semantics:** external delivery is at-least-once, so event/operation identity is durable and consumers must deduplicate.
- **Hard authorization boundaries:** `/public`, `/me`, `/admin`, `/staff`, `/commerce` and `/internal` are separate capabilities rather than one general bearer credential.
- **Measured scaling boundary:** the API is stateless apart from bounded process-local read caches; workers coordinate through PostgreSQL leases. A separate broker or partitioning is deferred until production measurements justify the extra system.
- **Release identity:** production deploys are tied to an exact Git/OCI revision and verified with readiness and end-to-end management checks.
- **Cross-repo compatibility:** OpenAPI and executable ecosystem contracts guard consumers such as Virya Signal, virya.music and Synesthesia against backend drift.

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the consistency model and deliberate trade-offs, and [`docs/STABLE_CONTRACT.md`](docs/STABLE_CONTRACT.md) for the compatibility policy.

## Workspace

| Crate | Responsibility |
| --- | --- |
| `crowdrelay-domain` | identifiers, events, value objects and deterministic policy |
| `crowdrelay-application` | use cases and repository/provider ports |
| `crowdrelay-infra` | PostgreSQL repositories, caches, provider adapters and observability |
| `crowdrelay-api` | HTTP/auth boundaries, validation and response contracts |
| `crowdrelay-worker` | outbox delivery, reminders, retention, synchronization and draws |

The intended dependency direction is domain → application → infrastructure/adapters, with transport and provider details kept outside business policy.

## Main command path

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

## Product surface

CrowdRelay currently covers:

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

The exact request/response/auth schemas live in [`openapi/openapi.yaml`](openapi/openapi.yaml).

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
| Autopilot | opportunities, Beacons, policy, approvals and operator read models |

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
- [`docs/ATTENDANCE_GROWTH_PLAYBOOK.md`](docs/ATTENDANCE_GROWTH_PLAYBOOK.md) — deterministic attendance-growth operating model
- [`docs/EXPERIMENTATION_PLAYBOOK.md`](docs/EXPERIMENTATION_PLAYBOOK.md) — bounded experimentation
- [`docs/operations/`](docs/operations/) — production/operator notes

## License

Apache-2.0. See [`LICENSE`](LICENSE).
