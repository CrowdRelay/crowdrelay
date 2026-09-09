# CrowdRelay

Durable business authority for the whole ecosystem. Rust workspace, Postgres owns state.

## North Star (read before any work)

**Grow real fans for the tenant (Virya is the first tenant).**

1. **Aggregate** fans from all sides of the internet — Reddit, Meta, Spotify,
   Bandsintown, forums, press, live shows — into the fanbase and Signal.
2. **Grow** them for real: genuine engagement, not spam.
3. **Convert** using fan 360 mechanisms: tickets, merch, attendance.

Every feature, every migration, every autopilot action must trace back to this goal.
Full plan: `/Users/wojciechbator/dev/AGENT_GROWTH_PLAN.md`

## Stack
Rust 1.97.1 (`rust-toolchain.toml`, edition 2024), Axum 0.8, SQLx 0.8 (Postgres),
Tokio 1, uuid v7, `time` (not chrono), tracing + JSON subscriber, rustls only.
Postgres 19 is asserted by a gate; do not downgrade assumptions.
**No compile-time SQLx macros** — only runtime `sqlx::query`/`query_as`, so no
`DATABASE_URL` and no `.sqlx` offline cache is needed to build or clippy.

## Layout (≈100k lines; do not `ls` around, start from here)
```
crates/crowdrelay-api          133 files / 52k lines  HTTP: auth boundary, validation, response contracts
crates/crowdrelay-infra        107 files / 54k lines  SQL, providers, config, observability
crates/crowdrelay-worker        61 files / 31k lines  outbox, draws, event_sync, push_delivery, retention, watchdog
crates/crowdrelay-domain        60 files / 28k lines  pure policy: pricing, referrals, admission, autopilot, funding…
crates/crowdrelay-application   50 files / 15k lines  use cases + ports; holds ZERO sqlx call sites (keep it that way)
crates/crowdrelay-brain         33 files / 18k lines  world model, strategy, causal model, opportunity/EFE, hypothesis lifecycle, walk-forward validation, metacognition
migrations/                    248 sequential .sql files (next = 0251_*)
openapi/openapi.yaml            the supported integration contract
docs/ARCHITECTURE.md            layering rules the ratchets enforce
crowdrelayctl/ deploy/ ops/ proofs/ integration/ n8n/
```
API module names map 1:1 to feature slices: `accounting`, `acquisition`, `admission`, `area`, `audience_graph`, `portfolio`,
`area_admin`, `audience`, `autopilot`, `beacon_signal`, `commerce`, `ecosystem`, `events`,
`fan_lifecycle`, `fan_privacy`, `mobile_fan`, `ops*`, `proofs`, `push`, `referrals`, `releases`,
`staff_sessions`, `synesthesia`, `ticketing`, `ticket_qr`, `concert_qr`, `tenant`.

## Route surfaces
**441 routes live in NINE files, not one.** `routing.rs` (993 lines, no policy in it) holds 283 of
them and `.merge()`s the rest. Grepping only `routing.rs` makes a live endpoint look unrouted —
that mistake has been made repeatedly, most recently against the whole ops timeline surface and
the autopilot dry-run preview, both of which are live in production.

```
routing.rs                       283   control_plane.rs                 101
area_admin.rs                     12   ops_routes.rs                     11
synesthesia.rs                    10   portfolio.rs                       8
audience_graph.rs                  6   community_intelligence_routes.rs   5
routing/growth.rs                  5
```

Prefix counts today, counted across all nine: `/v1/admin` 172, **`/v1/control-plane` 118**,
`/v1/internal` 51, `/v1/public` 34, `/v1/staff` 22, `/v1/me` 16, `/v1/beacon` 14.
These authority surfaces must never blur into each other.

Middleware, auth, request IDs and body limits stay centralized in `crates/crowdrelay-api/src/lib.rs`.
Authorization is path-prefix based there: `/v1/admin/` requires `PrivilegedAuthorization::Admin`,
`/v1/control-plane/` requires `ControlPlane`, and `is_control_plane_management_path` /
`is_area_management_path` carve narrower management scopes out of the control-plane surface. A new
route inherits its boundary from its prefix, so there is no per-route capability to forget — and
no way to widen authority by accident except by choosing the wrong prefix.

## Operator surfaces — check here before concluding a capability is missing

Six times in one session a capability was reported missing that was live in production. The
endpoints below are all registered and all answer 401, not 404. Probe before you conclude.

| Operator question | Endpoint |
| --- | --- |
| What needs me? | `/v1/control-plane/ops/attention` |
| What is the system doing? | `/v1/control-plane/ops/summary` |
| Why did it do this? | `/v1/control-plane/ops/trace/{trace_id}` |
| What happened to this request? | `/v1/control-plane/ops/operations/{request_id}` |
| What is the brain about to try? | `/v1/control-plane/autopilot/cycle/preview` |
| What is the autopilot's posture? | `/v1/control-plane/autopilot/posture` |
| What was sent, and did it arrive? | `/v1/control-plane/ops/outbox`, `/ops/deliveries` |
| What did the brain's last cycles do? | `/v1/admin/ops/cycles` (`?state=degraded`) |
| Which connections actually work? | `/v1/admin/ops/connections` |

`ops/attention` is the exception-first view: `needs_you`, `awaiting_approval`, `findings`, `brain`.
`brain` is the self-assessment — `improving` / `learning` / `stagnant` / `regressing` /
`initializing`, over a 60-day daily North Star series. `needs_attention` is true only for
`regressing` and `stagnant`; a flat count on a young system is `learning`, not a fault.
`ops/trace/{trace_id}` joins decision, action, outbox, delivery, measurement, evidence, reach,
audit and agent outcome into one timeline — every one of those rows carries the trace, and
`viryaos_autopilot_decisions.trace_id` is NOT NULL so a decision cannot be missing from it.
`cycle/preview` reports strategy and ranked template priority; it does not enumerate proposed
actions, which is the one part of a full dry-run that is genuinely not built.

The control plane UI consumes these — see `crates/` and `frontend/` in
`crowdrelay-control-plane`, not `src/`.

## Must preserve
- Postgres is authoritative for business state.
- One transaction commits: business rows + idempotency result + outbox intent.
- Provider delivery is async and at-least-once; consumers dedupe.
- Public API never waits for email/n8n/provider work.
- OpenAPI is the supported boundary; auth capability boundaries stay separate.

## Gates
```
just check   # cargo fmt --all; clippy --locked --workspace --all-targets --all-features -D warnings; cargo test --locked --workspace --all-targets --all-features
just ci      # check + validate-contract-assets + contract-tests + policy-checks
```
`policy-checks` runs 22 Python/bash policy scripts from `scripts/`; `contract-tests` is
`python3 -m unittest discover -s scripts -p 'test_*.py'`. Local DB: `just db-up`, `just migrate`, `just up`, `just health`. Live Postgres suite: `just test-postgres`.

**No compile-time SQL checking is the standing risk of the runtime-query choice.**
A query naming a table that does not exist compiles, lints and tests clean, then
fails on first request. Two gates cover the cheap half:
- `scripts/test_sql_identifiers_v1.py`: every relation named after FROM/JOIN/INSERT INTO/
  UPDATE/DELETE FROM must be created by a migration, bound as a CTE, or listed in
  `FOREIGN_RELATIONS` (tables another service owns). It does not check columns —
  `just test-postgres` does, against a real schema.
- `scripts/test_platform_vocabulary_v1.py`: the platform CHECK constraints and the
  `Platform` / `MetricPlatform` enums must agree, and a migration may never drop a
  value from a CHECK. `ADD CONSTRAINT` validates existing rows, so narrowing a
  constraint aborts the migration wherever such a row exists.

**Platform vocabulary has one source of truth per surface.** Connection platforms live in
`domain::fanbase::Platform`, metric-series platforms in `domain::growth_metrics::MetricPlatform`.
`parse`, `OFF_PLATFORM_FEEDS` and the worker's synced-platform list all derive from them —
add the variant and answer the match arms, do not retype a list.

## Ratchets — the most common self-inflicted CI failure
- `scripts/source-size-ratchet.py` + `.json`: any `.rs/.ts/.astro/.gd/.py` file over **1200 lines**
  must already be in the baseline. Growing a listed file past its recorded max fails.
- `scripts/api-sql-ratchet.py` + `.json`: counts **write** statements (INSERT/UPDATE/DELETE) in
  `crowdrelay-api`. Adding a write to the HTTP layer fails; move it behind a repository instead.
- `scripts/workspace-scope-ratchet.py` + `.json`: counts statements reading one of the 227
  `workspace_id`-bearing tables without naming the workspace. That column is the whole of the
  tenant isolation, so a new unscoped query fails. `--write-baseline` re-records.
All three may shrink freely. Raise a baseline only deliberately, in review — never as a reflex to get green.

## Debug first
Delivery: `request → tx → outbox → lease → attempt → provider → retry/delivered/dead → consumer dedupe`.
A 200 only proves request handling, not delivery. Outbox internals:
`crates/crowdrelay-worker/src/outbox/{worker,repository,transport,signature,backoff,secrets}.rs`;
it reads no env vars directly — callers pass validated policy and a `SecretProvider`.
Worker crashes: separate panic/process death vs connection loss vs lease race vs
retry amplification/resource exhaustion before redesigning anything.

## Performance
Fewer clones and allocations, bounded async concurrency, clean async control flow — but measure first
and never trade away transaction/idempotency semantics. Historical hot spots: SeaORM-era query paths,
Redis/JSON/search code, RabbitMQ/lapin consumers, reindex pipelines.

## Cross-repo
Changing an endpoint, auth shape, event or data shape ⇒ search consumers before merge:
`virya/src/server/*`, `virya/src/lib/crowdrelay*.ts`, `virya-signal/src-tauri/src/api`, `synesthesia`.
CI workflows: `.github/workflows/{ci,ecosystem-contract,external-proofs,performance,production-readiness,production-smoke,publish-images,security}.yml`.
