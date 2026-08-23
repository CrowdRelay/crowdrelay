# Growth Operating System — implementation plan

Multi-session plan. CrowdRelay already executes planned actions well; this work
adds the layer that decides *what is worth doing now*. It is deliberately split
into vertical slices that each ship value on their own, because the target end
state (observe → detect → recommend → execute → measure → learn) is not
something to land in one pass.

Read this file first when resuming. Every phase lists the exact files it
touches and the gate that proves it.

**Phases 1–4 are done and are the sensing layer.** The direction changed on
2026-08-23: the target is an autonomous growth agent, not a recommender. Read
"Direction change" before Phase 5 for the autonomy posture, then "Scope
addition" for the commercial half — gig economics, negotiation, target
discovery and the free-reach pitcher. Audit is Phase 17, control plane 18.

## Invariants for every phase

These are not negotiable and are cheaper to respect than to retrofit.

- PostgreSQL stays authoritative. No new broker, no new datastore, no new
  service, no Kubernetes. If something feels like it needs one, measure first
  and record the measurement here.
- One transaction commits business rows + idempotency result + outbox intent.
- Every ingestion endpoint is idempotent and replay-safe.
- `/v1/admin`, `/v1/internal`, `/v1/public`, `/v1/staff`, `/v1/me`, `/v1/beacon`
  stay separate authority surfaces.
- Every new detector runs under the existing Autopilot authority ladder
  (`observe` → `recommend` → `require_approval` → `bounded_auto`), the existing
  confidence gate, and the existing `max_actions_24h` quota. A detector never
  gets its own side channel to act.
- New contexts are provisioned **disabled** and at `observe`.
- The domain never claims a cause and never invents a provider capability. It
  reports what the evidence supports and says so when the evidence is thin.
- Vanity metrics never outrank a tracked downstream metric.
- `crowdrelay-application` keeps zero sqlx call sites.
- Writes never move into `crowdrelay-api` (the `api-sql-ratchet` enforces it).
- Files stay under 1200 lines unless already in `source-size-ratchet.json`.

Gates: `make check` while iterating, `make ci` before claiming a phase done.

---

## Phase 0 — inspection (DONE)

Findings that shape everything below:

- The decision/action machinery already exists and is good: 17 Autopilot
  contexts, `viryaos_autopilot_{policies,decisions,actions,action_attempts,
  action_emissions,execution_claims,execution_reports,measurements,outcomes}`,
  a typed `DecisionCandidate` with `decision_key` + `action_idempotency_key`,
  and `AutonomyLevel`/`Confidence`/`PolicyDisposition` in
  `crowdrelay-domain/src/autonomy.rs`. **Do not rebuild any of this.**
- `viryaos_city_market_signals` is TTL'd, city-scoped market intelligence —
  not a metric time series. It answers "which city looks warm", not "did our
  YouTube growth stall". Both are needed; they are different tables.
- `load_chief_of_staff` (`crowdrelay-infra/src/autopilot/operations/chief.rs`)
  is already two thirds of an operator brief: 24h executed/failed/awaiting
  counts, an estimated-minutes-saved rollup, a top-opportunity list from recent
  decisions, and a deadline radar. Phase 6 extends it; it does not replace it.
- `viryaos_autopilot_measurements` + `viryaos_autopilot_outcomes` already carry
  `effect_assessment` in (`improved`,`neutral`,`worsened`) per action. Phase 5
  extends the measurement kinds; it does not build a second attribution system.
- The pattern to copy for anything new is `show_growth` (migration 0049): one
  migration extends three context CHECK constraints and the provisioning
  trigger, one domain module holds the rule, one `evaluate/*.rs` maps a domain
  decision to a `DecisionCandidate`, one `operations/*.rs` holds the snapshot
  query.

---

## Phase 1 — external metrics + trend/anomaly + `growth_metrics` context (DONE)

The foundation. One normalized model, one detector, wired into the existing
authority ladder.

### 1a — schema and domain (DONE)

- `migrations/0073_viryaos_growth_metrics.sql`
  - `viryaos_growth_metric_series`: operator-declared identity of one tracked
    number (`platform`, `metric_key`, optional `subject_kind`/`subject_id`,
    `direction`, `value_tier`, `expected_interval_hours`, `active`).
  - `viryaos_growth_metric_points`: append-only absolute observations, unique
    on `(workspace_id, series_id, captured_at)`.
  - Adds `growth_metrics` to the policy/decision/action context constraints and
    to `viryaos_provision_autopilot_policies()`.
  - Deltas/baselines are **not** stored. They are derived, so a backfill or a
    re-ingest can never leave a stale derived row behind.
- `crates/crowdrelay-domain/src/growth_metrics.rs`
  - `MetricPlatform`, `MetricDirection`, `MetricValueTier`, `MetricPoint`.
  - `compute_trend()`: 24h/7d/28d deltas, 7d velocity, a 21-day baseline that
    excludes the last 7 days (so a live anomaly cannot raise its own bar),
    window coverage, and head age. Absent history reports `None`, never `0`.
  - `evaluate_growth_metric()`: dead feed → thin coverage → vanity deferral →
    reversal → stall/surge ratio, with an absolute-movement floor and a
    cooldown. Emits `GrowthOpportunity { signal, confidence, priority,
    deviation_basis_points }`.
  - Integer arithmetic only; rates in milli-units/day, comparisons in basis
    points.
  - 14 unit tests covering each branch, both directions, and the priority
    ordering between value tiers.
- `GrowthMetricSeriesId` added to `ids.rs` and re-exported from `lib.rs`.

### 1b — application (DONE)

- `crates/crowdrelay-application/src/autopilot/model.rs`
  - `AutopilotContext::GrowthMetrics` → `"growth_metrics"`.
  - `AutopilotPolicyConfig::GrowthMetrics(GrowthMetricPolicy)`.
  - `ActionSubject::GrowthMetricSeries(GrowthMetricSeriesId)` →
    `"growth_metric_series"`.
  - `AutopilotActionPayload::RaiseGrowthOpportunity { series_id, platform,
    metric_key, signal, recommended_action, deviation_basis_points, priority,
    template_key }` → `action_kind` `"growth.opportunity.raise"`.
- `crates/crowdrelay-application/src/autopilot/ports.rs`
  - `load_growth_metric_snapshots(workspace_id, now)` on
    `AutopilotDecisionRepository`.
- `crates/crowdrelay-application/src/autopilot/evaluate.rs` + new
  `evaluate/growth_metrics.rs`
  - `growth_metric_candidate()`, mirroring `show_growth_candidate`.
  - `decision_key` must change when the evidence changes (include policy
    version, series id, signal, deviation bucket, latest value).
  - `action_idempotency_key` must be stable per (series, signal, cooldown
    window) so a re-detect inside the cooldown cannot enqueue twice.
- New `AutopilotGrowthMetricsRepository` port (separate trait, matching the
  existing split-port style) for ingest + read model:
  - `upsert_growth_metric_series(...)` → idempotent via `operator_actions`.
  - `record_growth_metric_point(...)` → idempotent; conflicting `captured_at`
    is a no-op replay, not an error.
  - `load_growth_metric_trends(...)` → read model for the API.

### 1c — infrastructure (DONE)

- `crates/crowdrelay-infra/src/autopilot/growth_metrics.rs` (new): the two
  writes (both through `insert_operator_action`, same as
  `upsert_city_market_signal` in `state.rs`), the trends read model, and the
  snapshot loader.
- `crates/crowdrelay-infra/src/autopilot/operations/growth_metrics.rs` (new):
  one set-oriented query returning every active series with its 28-day point
  window, `hours_since_last_signal` (from `viryaos_autopilot_decisions`), and
  `stronger_tier_tracked` (a peer series on the same platform at a strictly
  higher `value_tier`). Build the `MetricTrend` via `compute_trend` in the
  adapter — SQL returns points, the domain does the arithmetic.
- `crates/crowdrelay-infra/src/autopilot/mapping.rs`: `"growth_metrics"` arm in
  `parse_policy` + `GrowthMetricPolicy::default()` config parse.
- `decisions.rs` + `decisions/opportunity_reads.rs`: forward the loader.
- `crates/crowdrelay-infra/src/autopilot.rs`: module + re-exports.

### 1d — API and contract (DONE)

- `POST /v1/admin/autopilot/growth-metrics/series` — declare/update a series.
- `POST /v1/admin/autopilot/growth-metrics/points` — ingest one observation.
  Requires `Idempotency-Key`. Rejects `captured_at` in the future beyond the
  existing clock-skew allowance, negative values, unknown platform/tier, and
  bodies over the standard limit.
- `GET /v1/admin/autopilot/growth-metrics/trends` — derived read model:
  latest, 24h/7d/28d deltas, velocity, baseline, ratio, coverage, staleness.
  Explicitly reports `null` for windows without evidence.
- Handlers in `crates/crowdrelay-api/src/autopilot/growth_metrics.rs`,
  validation in `autopilot/validation.rs`, request shapes in
  `autopilot/requests.rs`, registration in `routing.rs`.
- `openapi/openapi.yaml`: the three operations plus schemas.
- Batch ingestion is deliberately out of scope until a provider needs it.
- Contract surfaces that had to move with the new context, for the next time one
  is added: `SCHEMA_VERSION` in `crates/crowdrelay-api/src/meta.rs` (and its own
  assertion), `parse_context` in `autopilot/validation.rs`, the `AutopilotContext`
  enum and `AutopilotOverview.policies.maxItems` in `openapi/openapi.yaml`, the
  action-kind enum in the same file, and the context count in
  `scripts/test_viryaos_autopilot_v1.py`.
- Note for anyone running the gate locally: `scripts/test_openapi_router_coverage.py`
  and `scripts/test_release_hardening_20260819.py` need PyYAML. Without it they
  report an import error that has nothing to do with the code under test.

### 1e — proof (DONE)

- `make check` and `make ci` green.
- `scripts/test_growth_metrics_v1.py` (8 tests): the tables exist, observations
  are unique per capture time, no derived movement is stored beside the
  observations, every provisioning statement supplies only the quota so the
  context arrives disabled and observing, and the three database context CHECK
  constraints match `AutopilotContext` exactly.
- `crates/crowdrelay-application/src/autopilot/evaluate/growth_metrics_tests.rs`
  (7 tests): a `recommend` policy never produces `auto_execute`, the decision
  key changes with the evidence and with the policy version, the action key is
  stable inside a cooldown window and different across windows, a mismatched
  policy config yields no candidate, and confidence below the floor is denied.

---

## Phase 2 — real metric sources (DONE)

Model first, sources second. The two slices that shipped are both first-party,
because the strongest metrics in the system need no provider at all.

### What landed

- `materialize_first_party_growth_metrics` on a dedicated
  `AutopilotFirstPartyGrowthMetrics` port, implemented in
  `crates/crowdrelay-infra/src/autopilot/growth_metrics.rs`, called by the
  Autopilot worker cycle **before** evaluation so a cycle reasons about the
  newest evidence it can. It is its own phase: a metric write failing must not
  stop already-authorized work.
- **Slice 1 — per-event ticketing.** `ticketing/paid_tickets` and
  `ticketing/paid_buyers`, subject `event`, tier `downstream`. Declared for
  published or completed events from 30 days past to 365 days ahead, and
  **retired** (`active = false`) once an event ages out of that window — an
  abandoned series would otherwise be reported as a dead feed forever.
- **Slice 2 — workspace totals.** `signal/active_fans` and `merch/paid_orders`,
  no subject, tier `downstream`. These proved the NULL-subject path, which is
  what surfaced the uniqueness bug below.
- Observations are bucketed to the top of the hour and inserted with
  `ON CONFLICT DO NOTHING`. The worker cycle is far shorter than an hour, so
  without a bucket the window would describe our polling rate rather than the
  business. `expected_interval_hours` is 6 while points are written hourly, so
  a worker restart or short outage does not read as a dead feed.
- Retention step `expired_growth_metric_points` deletes observations older than
  90 days. The derived window is 28 days plus a 7-day tolerance, so anything
  older cannot influence a trend; 90 days leaves room to widen the window later
  without having already destroyed the evidence.

### Bug found and fixed while doing this

`viryaos_growth_metric_series` originally had a plain
`UNIQUE (workspace_id, platform, metric_key, subject_kind, subject_id)`. Under
default NULL semantics two subject-less series for the same metric do not
conflict, so `ON CONFLICT` never fired and the second upsert of a
workspace-level series hit a primary-key violation instead of updating.
Migration 0073 now uses `UNIQUE NULLS NOT DISTINCT`. It was amended in place
rather than patched by an 0074 because it had never run anywhere outside this
branch. A contract test pins it.

### Bandsintown — deliberately not done, and why

`crates/crowdrelay-worker/src/event_sync/bandsintown.rs` calls exactly one
endpoint, `/artists/{artist}/events`, and its response carries event data only:
id, url, datetime, title, description, lineup, venue, offers. There is no
tracker or follower count in anything this repository actually receives.

Follower/tracker counts live on a different endpoint (`/artists/{artist}`) that
this codebase does not call. Adding it is a deliberate provider change — a new
request, a new failure mode, and a new field whose semantics need confirming
against real responses — not something to assume because the number would be
convenient. Until someone does that work with a real response in hand, there is
no Bandsintown series. A provider that cannot supply a number gets no series,
not a series full of zeroes.

Spotify, YouTube and social continue to arrive through the Phase 1d ingest
endpoint driven by the existing n8n adapters. CrowdRelay does not grow OAuth
flows for them until there is a reason it must own the credential.

## Phase 3 — growth debt detectors (DONE, except the blocked `StaleContactData` kind)

Neglected work is the other half of the opportunity engine, and most of the
inputs already exist as first-party rows.

- Warm relationship gone quiet: `viryaos_beacons` /
  `viryaos_booking_targets` / `viryaos_outreach_targets` with a positive
  relationship score and no interaction in N days (`viryaos_beacon_*`,
  `viryaos_booking_interactions`, `viryaos_outreach_interactions`).
- Event missing required growth actions: an upcoming event whose
  `viryaos_show_growth_surfaces` history has unrequested levers past their lead
  time. Most of this rule already exists in `show_growth`; the debt view is the
  *aggregate* of what was skipped, not a second copy of the rule.
- Incomplete distribution: a release plan whose milestones stopped being
  recorded. **Correction to the original bullet:** this is
  `viryaos_release_plans` + `viryaos_release_milestones` (migration 0039).
  `viryaos_release_components` (migration 0040) is the deploy/CI component
  ledger for *software releases* and has nothing to do with a music release
  plan. Do not join it here.
- Inactive channel: a `growth_metrics` series that has gone flat for longer
  than its cadence — already emitted as `StaleFeed` in Phase 1. Not repeated in
  this context.
- Stale information: `viryaos_*_targets` rows whose verified contact data is
  older than the policy horizon. **Blocked** — see the open question below.

### Decision — one `growth_debt` context, against the stated default

The plan's default was to extend existing contexts. Rejected, for three
reasons recorded here so it is not re-litigated:

1. Authority. `outreach`, `booking_opportunity` and `release` execute
   contractual, outward-facing work and are gated for that. Raising debt is an
   observation about our own records and is safe far wider. Folding it in would
   either widen those contexts' authority to cover a cheap observation or
   throttle the observation behind a quota sized for paid outreach.
2. Quota. How often an operator wants to hear about neglect is a different
   number from how many emails may go out. `max_actions_24h` cannot express
   both at once.
3. Phase 4. One action kind (`growth.debt.raise`) across every debt kind gives
   the ranked queue one comparable stream instead of four look-alike predicates
   spread over three contexts.

The context stores nothing of its own. Migration 0074 creates no table: debt is
derived at evaluation time from the tables that already own the facts, exactly
like a `growth_metrics` trend.

### 3a — schema, domain rule and context registration (DONE)

- `migrations/0074_viryaos_growth_debt.sql` — no tables. Extends the three
  context CHECK constraints, the provisioning trigger and the backfill insert.
  Quota 10/day, provisioned disabled and at `observe`. `subject_kind` on the
  decision/action tables is free-form bounded text (0033), so the new
  `booking_target` / `outreach_target` subjects need no constraint change.
- `crates/crowdrelay-domain/src/growth_debt.rs` — `GrowthDebtKind`,
  `GrowthDebtSubject`, `GrowthDebtObservation`, `GrowthDebtPolicy`,
  `GrowthDebtItem`, `evaluate_growth_debt()`. The adapter supplies facts
  (`idle_hours`, `outstanding_items`, `tracked_items`, dates); every horizon
  and threshold lives in the policy, so changing what counts as neglect never
  means changing a query. 15 unit tests.
- Two refusals are load-bearing and pinned by tests: debt whose deadline has
  passed is dropped (a show that already played cannot be promoted), and debt
  is never claimed from an empty denominator (`tracked_items == 0` is a
  statement about our records, not the business).
- `value_tier()` reuses `growth_metrics::MetricValueTier` on purpose — one
  ordering decides what outranks what across both detectors, which is how the
  "vanity never outranks downstream" invariant stays true between them.
  `MetricValueTier::weight()` is now `pub(crate)`.
- Context registered across the Phase 1d surfaces: `AutopilotContext::GrowthDebt`
  and `AutopilotPolicyConfig::GrowthDebt` in `model.rs`, `parse_policy` +
  `parse_context` in `mapping.rs` / `validation.rs`, `SCHEMA_VERSION` 73 → 74,
  the OpenAPI `AutopilotContext` enum, `AutopilotOverview.policies.maxItems`
  18 → 19, and the context count in `scripts/test_viryaos_autopilot_v1.py`.
- `scripts/test_growth_debt_v1.py` (18 tests after 3b/3c).

Found and fixed while doing this, both worth not rediscovering:

- **Overflow in the outstanding-share ratio.** `outstanding * 10_000` in `u32`
  saturates past ~429k items, and a saturating multiply then divides a clamped
  numerator by a real denominator — a fully neglected subject would report as
  ~0% outstanding and be held. Now computed in `u64`. Pinned by a test.
- **`AutopilotContextPath` in `openapi/openapi.yaml` had drifted.** It inlined
  its own copy of the context enum and never got `show_growth` or
  `growth_metrics`, so the published contract rejected path values the API has
  accepted since Phase 1. It now `$ref`s `AutopilotContext`, which makes the
  drift structurally impossible; a test asserts it holds no inline `enum`.
  **Add this parameter to the Phase 1d contract-surface list.**
- `scripts/test_growth_metrics_v1.py` asserted its own migration's constraints
  *equal* the Rust enum. A migration is history and may legitimately be behind
  a later one, so it now asserts subset; the newest context migration owns the
  equality claim.

### 3b — application (DONE)

- `load_growth_debt_observations(workspace_id, now)` added to the existing
  `AutopilotDecisionRepository` rather than a new trait. One loader does not
  justify a port of its own, and `decisions.rs` is 214 lines — nowhere near the
  size ratchet, which was the only reason to consider splitting.
- `evaluate/growth_debt.rs` with `growth_debt_candidate()`, mirroring
  `growth_metric_candidate`. `decision_key` carries policy version, subject
  kind and id, debt kind, overdue bucket and outstanding count;
  `action_idempotency_key` is stable per (subject, debt kind, cooldown window)
  and deliberately omits the overdue ratio so ordinary ageing cannot stack
  duplicates on the operator queue.
- `AutopilotActionPayload::RaiseGrowthDebt` → `action_kind`
  `"growth.debt.raise"`. One action kind for every debt kind, so the Phase 4
  queue sees one comparable stream.
- `ActionSubject::BookingTarget` and `OutreachTarget` did not exist and were
  added; `Beacon`, `Event` and `ReleasePlan` already did. `From<GrowthDebtSubject>`
  keeps the mapping in one place.
- 9 evaluator tests in `evaluate/growth_debt_tests.rs`.

Two things worth not rediscovering:

- **`subject_kind` on the payload cannot be `&'static str`.**
  `AutopilotActionPayload` is deserialized back out of the durable action row,
  and a borrowed field makes the derived `Deserialize` valid only for
  `'static`, which fails at the `serde_json::from_value` call site in
  `infra/autopilot/actions.rs`. It is a `String`.
- **Correction to the Phase 1d contract-surface list: there is no action-kind
  enum in `openapi/openapi.yaml`.** `action_kind` is a free-form
  `type: string, maxLength: 96` in three schemas. Nothing to update for a new
  action kind beyond staying inside 96 characters.

### 3c — infrastructure (DONE)

`operations/growth_debt.rs`: three debt kinds in one `UNION ALL`, one round
trip, capped by `MAX_SNAPSHOTS_PER_CONTEXT`, plus one grouped query for the
cooldown. No per-subject N+1. The SQL reports facts only — a ratio or priority
computed there would be a second copy of the rule that drifts silently.

- Relationship quiet: `viryaos_booking_targets` and `viryaos_outreach_targets`.
  `viryaos_beacons` is **not** included: it has no interaction log of its own
  and no relationship score, so the rule would have neither of the two facts it
  needs. `tracked_items` is 1.
- Event levers: `viryaos_show_growth_surfaces` for published events still
  ahead, counting statuses in (`unknown`,`ready`,`manual`,`blocked`) as
  outstanding against every declared surface. `skipped` and `retired` are
  excluded — those are decisions somebody made, and counting them would report
  deliberate choices as neglect.
- Release milestones: `viryaos_release_plans` where `active` and still ahead,
  against `viryaos_release_milestones`. The milestone CHECK list (8 values) is
  the denominator, not the recorded rows: counting recorded rows would make a
  plan that stopped after one milestone report as 0% outstanding.
- **Idle clock uses `GREATEST(...)` over every timestamp that exists**, with
  the row's `created_at` as the floor. Postgres `GREATEST` ignores NULLs, so it
  never collapses. Reading only the interaction log dated a target that has
  `last_outreach_at` but no logged interaction from its creation, which
  overstates the neglect; the first draft had that bug.
- **Cooldown is read back per (subject, decision kind), not per subject.** One
  event can owe both skipped levers and a stalled release plan, so
  `GrowthDebtKind::decision_kind()` returns a per-kind string and the last
  signal query groups on it. Raising one debt must not silence the other for a
  fortnight.

**Not runtime-verified.** The Docker daemon was not running on the machine that
wrote this, so `make db-up && make migrate` could not execute the query against
a real Postgres. It compiles and the contract tests hold, but the first run
against a live database is still ahead — check it before trusting a row count.

### 3d — operator visibility (DONE, and scoped down deliberately)

The planned `GET /v1/admin/autopilot/growth-debt` read model was **not** built,
and the reasoning belongs here rather than being re-argued next run:

- Debt decisions and actions already surface through the existing
  `AutopilotOverview` (`recent_decisions`, `recent_actions`, `queued_actions`),
  because those are context-agnostic. Nothing had to be added for an operator
  to see a raised debt item.
- The one place a new context is genuinely invisible is the chief-of-staff
  opportunity query, which filters by an explicit context allow-list. Both
  `growth_metrics` and `growth_debt` were missing from it — so Phase 1's
  detector has been producing decisions nobody would have seen in the brief.
  Both are now in the list (this is Phase 4's first bullet, pulled forward
  because it is one line and the alternative was shipping a blind detector).
- A dedicated per-context endpoint would be a third read surface over the same
  rows, and Phase 4's ranked queue is meant to subsume exactly that. Building
  it now means maintaining and then deprecating it.

If an operator later wants debt on its own, separate from the mixed queue, the
endpoint is a thin read over rows that already exist. Until someone asks, it is
not worth the contract surface.

### 3e — proof (DONE)

- `make check` green. `make ci`: 397 contract tests with only the two known
  PyYAML import errors, every `runtime-contracts` script PASS, both ratchets
  PASS (`source-size` tracked=20 currently_large=0, `api-sql` writes=129 =
  baseline, headroom 0 — the loader is a read, so it did not move).
- 15 domain unit tests, 9 evaluator tests, 19 contract tests.
- Still outstanding for this phase: one live-database run of the observation
  query, and the `StaleContactData` decision below.

### Open question for the operator — blocking `StaleContactData`

The rule is written and tested but **no adapter can supply it**, because the
schema has no verification timestamp: `viryaos_outreach_targets.verified` and
`viryaos_beacons.verified` are booleans, and `updated_at` moves whenever any
column does, so reading it as "last confirmed" would fabricate evidence.

Two ways forward, and it is an authority decision, not a technical one:

1. Add `contact_verified_at timestamptz` to the target/beacon tables, NULL for
   every existing row (NULL = never verified = the rule holds, which is already
   its behaviour). Then decide **what writes it**: a staff endpoint, an operator
   action, or a side effect of a recorded inbound reply.
2. Drop the kind until a real verification workflow exists.

Until this is answered, 3c wires the three kinds that have a defensible clock
and leaves `StaleContactData` unreachable.

---

## Phase 4 — one prioritized Next Best Action queue (DONE)

`GET /v1/admin/autopilot/next-best-actions`, admin-only, no filters and no page
size — the domain caps it at 10, so there is nothing for a caller to get wrong.

### The ranking is lexicographic, not a weighted score

This is the decision worth not re-litigating. A weighted sum is easy to write
and impossible to explain: an operator cannot tell why a suggestion landed
where it did, a small weight change silently reorders everything, and a good
past record can buy its way past a live deadline. Ordered tiers let every entry
name the single factor that decided its position against its neighbour, which
is also what makes the Phase 7 adjustment auditable rather than magic.

Order, highest first, exactly as this plan fixed it: authority state, deadline
proximity, value tier, measured effect, confidence, deviation magnitude. Ties
break on `(subject_id, decision_kind)` so the same evidence always produces the
same queue — one that reshuffles between two reads is one an operator stops
trusting.

### Files

- `crates/crowdrelay-domain/src/next_best_action.rs` — `AuthorityState`,
  `MeasuredEffect`, `QueueCandidate`, `RankFactor`, `RankedAction`,
  `rank_next_best_actions`, `MAX_QUEUE_ENTRIES = 10`. 13 unit tests.
- `crates/crowdrelay-application/src/autopilot/control.rs` — the `NextBestAction`
  view; `control/runtime_ports.rs` — `load_next_best_actions` on
  `AutopilotControlRepository`, beside `load_chief_of_staff`.
- `crates/crowdrelay-infra/src/autopilot/operations/next_best_action.rs` — one
  query over decisions + their newest action + the subject's own date, capped at
  200 candidates before ranking.
- `crates/crowdrelay-api/src/autopilot.rs` + `routing.rs`, `openapi/openapi.yaml`
  (path + `NextBestAction` schema), `scripts/test_next_best_actions_v1.py`
  (14 tests).

### Decisions taken while building it

- **Auto-executing work ranks last, not first.** Nobody is blocked on it. The
  queue is a human's list, and surfacing already-handled work at the top is how
  a queue becomes noise.
- **Denied decisions never enter.** The gate refused them; listing them as work
  to do invites overriding a policy from a list view.
- **An expired deadline ranks below every live one**, above nothing. It cannot
  be met, and putting unrecoverable work above recoverable is the worst possible
  ordering.
- **An unknown value tier ranks as `Intermediate`, not `Vanity`.** Absent
  evidence is not evidence of low value.
- **Only the newest decision per (subject, decision kind) is shown.** Every
  cycle writes a new decision row when the evidence moves; without this the
  queue fills with the same finding.
- **Deadlines are only ever real dates** — `events.starts_at`,
  `viryaos_release_plans.release_at`, `viryaos_team_opportunities.deadline`, or
  the action's own `approval_expires_at`. No fallback, no synthesized urgency.
- **Expected impact is a measured deviation in basis points, never a currency
  amount.** The system does not know what a stalled channel is worth, and a
  plausible figure would be the most convincing lie in the whole response.
- `measured_effect` is wired into the comparator but always `None` until Phase 5
  records growth outcomes. The slot exists so Phase 7 changes *data*, not the
  comparator — reordering the tiers later would invalidate every explanation an
  operator had already read.

### Correction to the Phase 1d contract-surface list

`SCHEMA_VERSION` in `crates/crowdrelay-api/src/meta.rs` tracks **the latest
migration number**, not the API surface. Six contract tests assert it. Bumping
it for an endpoint that ships no migration fails the gate; leaving it alone when
a migration lands fails the same tests. It is 74 today, matching migration 0074.

### Not runtime-verified

Same gap as Phase 3c: no Docker daemon on the machine that wrote this, so the
query has never run against a real Postgres. `make db-up && make migrate`, then
read the queue on real VIRYA data, before trusting it.

## Direction change — 2026-08-23

Phases 1–4 built a system that **senses and recommends**: it knows which numbers
moved, which committed work was abandoned, and what a human should do next. It
never acts. That is not the target. The target is an autonomous growth agent
that actively grows the band.

Two decisions from the operator set the shape of everything below.

**Autonomy: safest real autonomy first, expansion designed in.** The agent acts
on its own toward its own audience — consented fans, first-party surfaces,
links, segments, timing. Anything that touches a third party (venue, curator,
promoter, press) or costs money stays approval-gated. This is not a limitation
to work around: the owned-audience surface is where most honest growth actually
comes from, and it is the surface where a mistake is recoverable.

**Objective: Spotify and Bandsintown.** Followers, saves, trackers. Grown by
asking real people and by putting real work in front of real curators.

> **No fabricated engagement, ever.** No purchased or botted listens, follows,
> plays or trackers, no click farms, no incentivised streaming, no artificial
> repeat play. A number that did not come from a person who chose is worthless
> to the band and poison to the platform relationship. Any play that cannot
> explain which real person did the thing and why does not ship.

Phase numbering changed here. The audit and control-plane phases, previously 8
and 9, are now **13 and 14**.

---

## Additional invariants from this point on

On top of the invariants at the top of this file, which all still hold:

- **Autonomy is decided by an action's cost and reversibility, not only by its
  context.** The existing per-context ladder stays; a second ceiling sits above
  it, and the stricter of the two wins.
- **The agent never claims a fan did something it cannot observe.** Spotify
  follows and Bandsintown trackers arrive as workspace-level series. We can
  observe that we asked 2,000 people and that the series moved. We cannot
  observe who followed. Saying otherwise is fabricated attribution.
- **Every outward touch is capped, cooled down and reversible in intent.** No
  play may exceed its weekly budget, contact a fan inside their cooldown, or
  send outside quiet hours.
- **A play that cannot be stopped is not a play.** Every one has a kill switch
  and a stop rule.
- The agent is never the only thing that can act. Every autonomous step is
  visible in the operator brief before, during and after.

---

## Phase 5 — autonomy envelope (DONE)

Nothing acts until the envelope exists. This is the phase that makes "the agent
sent that on its own" a sentence an operator can hear without alarm.

### 5a — action class and the effective-authority ceiling (DONE)

- `crates/crowdrelay-domain/src/action_class.rs`: `ActionClass` in
  (`first_party_reversible`, `owned_audience`, `third_party`, `paid`).
  - `first_party_reversible` — own listings, smart links, referral codes,
    segments, drafts, internal scheduling. Costs nothing, touches nobody,
    undoable.
  - `owned_audience` — email, push and in-app to consented fans. Costs nothing,
    touches people who opted in, not undoable once sent.
  - `third_party` — venue, promoter, curator, press, partner. Reputational and
    irreversible; a bad one closes a door permanently.
  - `paid` — ads, reorders, shipping, price changes.
- Every `AutopilotActionPayload` variant maps to exactly one class. A new
  payload without a class must not compile — that is the point of the mapping
  living in a `match` rather than a lookup table.
- `effective_authority(context_level, class_ceiling)` returns the **stricter**
  of the two. A context at `bounded_auto` emitting a `third_party` action still
  requires approval.
- The ceiling table is **operator data, not code** (`viryaos_growth_autonomy`),
  seeded to the safest posture: `first_party_reversible` → `bounded_auto`,
  `owned_audience` → `bounded_auto`, `third_party` → `require_approval`,
  `paid` → `require_approval`. Widening later is a row update plus template
  pre-approval, not a rewrite. That is the whole expansion story.

**As built.** `action_class.rs` (13 unit tests), migration
`0075_viryaos_growth_autonomy.sql`, `AutopilotActionPayload::action_class()`,
`load_autonomy_ceilings` on `AutopilotDecisionRepository`, and the clamp applied
in `EvaluateAutopilot::persist`. `scripts/test_growth_autonomy_v1.py`,
12 contract tests. `SCHEMA_VERSION` 74 → 75.

Decisions worth not re-deriving:

- **The clamp lives in `persist`, not in the candidate functions.** Every one of
  the twenty dispatch arms reaches the database through that one method, so a
  new detector cannot forget the ceiling and its author cannot choose to skip
  it. A contract test asserts `clamp_disposition` appears exactly once.
- **`clamp_disposition` only ever downgrades, and never reopens a denial.** The
  ceiling answers "how far is the agent allowed to go", not "how far should it
  go"; a confidence gate that refused is not a permissions question.
- **A missing or unreadable ceiling row falls back to `safest_ceiling()`**, not
  to unlimited authority. An unrun migration must never be a grant of authority,
  and an authority row this build cannot parse is not a reason to guess
  permissively.
- **`RequestShowGrowth` and `ExecuteReleaseMilestone` are classified per lever
  and per milestone**, not per variant. One class for the whole variant would be
  wrong in both directions: it would gate a push to our own fans, or let
  `start_press` go out unattended.
- **Ticket and merch price changes are `paid`.** Changing what a customer pays
  is not recoverable by changing it back — somebody already paid the other
  number.
- **`SendTeamAssignmentEmail` is `first_party_reversible`.** It reaches our own
  staff; charging internal task routing to the audience budget would let admin
  traffic silence a fan message.
- **`Paid` is not `is_outward`.** Spend is capped by money, not by the
  outward-touch budget — counting an ad buy against the message budget would let
  it silence a newsletter.
- Cycle reports now count `actions_gated` separately from `actions_throttled`:
  throttled work is deferred, gated work is somebody's decision to make.

### 5b — the envelope itself (DONE)

Per-workspace, operator-editable, with defaults that are deliberately timid:

- weekly outward-touch budget per channel (email, push, in-app)
- per-fan cooldown — no fan hears from the agent twice inside it, whatever the
  play
- quiet hours, honouring the existing tenant timezone and push preference
  contract rather than inventing a second one
- blast radius — maximum recipients in one step, so a bad segment costs tens of
  sends and not thousands
- a global kill switch that stops the agent without touching the rest of
  Autopilot
- **dry run**: the agent produces the exact steps, segments and copy it would
  execute, and executes nothing. This is how a new play earns trust.

**As built.** `crates/crowdrelay-domain/src/growth_envelope.rs` (11 unit tests),
migration `0076_viryaos_growth_envelope.sql`, `load_growth_envelope` and
`load_outward_touch_ages` on `AutopilotDecisionRepository`, applied in
`EvaluateAutopilot::persist` immediately after the class clamp.
`scripts/test_growth_envelope_v1.py`, 16 contract tests. `SCHEMA_VERSION`
75 → 76.

Decisions worth not re-deriving:

- **No new ledger.** Outward touches are already durable rows in
  `viryaos_autopilot_actions`, so the envelope counts those. A parallel ledger
  would be one more thing that can disagree with the actions it describes. The
  cost of this is one new column, not one new table.
- **`viryaos_autopilot_actions.action_class` is written at insert and is
  nullable.** Written rather than derived, because deriving it at read time
  means reimplementing the Rust classification in SQL and the two drift the
  first time a lever is reclassified. It also records the class the action was
  *authorised under*, which is what an audit needs. NULL means the row predates
  the envelope, and that work is deliberately not charged to the agent — it was
  not the agent's.
- **Cancelled actions are not counted as touches.** An approval somebody refused
  never reached anybody. Failures still count: a send that errored may still
  have gone out.
- **The kill switch stops outward contact only.** First-party work is exempt, so
  switching the agent off is a stop on contact rather than a rollback of
  housekeeping.
- **`agent_enabled` and `dry_run` are separate.** Turning the agent on must not
  also be the moment it first sends something real.
- **Dry run is the one block that produces nothing approvable** — it downgrades
  to `recommend`, everything else to `require_approval`. An approve button in a
  rehearsal view turns the rehearsal into a send.
- **Budget is per outward class.** A busy newsletter week must not silence
  curator outreach, and a wave of pitches must not eat the audience budget.
- **The cooldown is keyed on subject, not on fan.** A superset of the intended
  rule and cheaper to enforce: no subject hears from the agent twice inside the
  window, whichever play wants to reach them.
- Both counting queries are time-bounded (7 days, 365 days) and covered by two
  partial indexes, and both are read once per cycle rather than per candidate.

**Deferred, with the reason.** The plan said "weekly budget *per channel*".
There is no channel column on `viryaos_autopilot_actions` — the channel is
implied by the action kind and the executor. Budgets are per action class for
now; splitting email from push means adding a channel to the action row, which
is a bigger change than it looks and is not worth it until a play exists that
would be throttled wrongly by the coarser bucket.

**Not runtime-verified.** No Docker daemon on this machine, so neither
migration has run against a real Postgres. Do that before switching
`agent_enabled` on anywhere.

### 5c — proof (DONE)

All four properties this phase named are pinned: a `third_party` payload cannot
reach `auto_execute` while its ceiling says otherwise, the kill switch stops
outward scheduling within one cycle, dry run produces no approvable action and
therefore no outbox intent, and the weekly budget cannot be exceeded.

**The budget one was a real bug, found by writing the proof.** The envelope is
loaded once per cycle, so the spend it carried never moved: a single cycle with
fifty findings would have enqueued all fifty against a budget of five, and every
one of those is a send nobody authorised. The spend is now topped up as actions
are created, so the cap holds *within* a cycle and not only between cycles.
Pinned by a contract test.

Totals across Phase 5: 24 domain unit tests, 29 contract tests, `make check`
green, contract suite green apart from the two known PyYAML import errors.

---

## Scope addition — 2026-08-23 (second pass)

The operator added the commercial half. The agent is not only to grow numbers;
it is to find and win **free reach** and **gigs on terms that actually pay**.

What survives from the existing build, unchanged and reused rather than rebuilt:

- `crates/crowdrelay-domain/src/live_opportunities.rs` already holds the show
  budget the operator described: `annual_target` 15, `annual_stretch` 20, a
  stretch show requiring a higher score, a `FarShot` travel band requiring a
  higher score again, `net_margin = expected_fee - estimated_cost -
  application_fee`, and an auto-submit gate that refuses contracts, exclusivity
  and negative margin. **Do not rebuild this.**
- `OutreachTargetKind` already covers `Playlist`, `Radio`, `Press`, `Creator`,
  `SupportSlot`, `Endorsement`, `MediaPatronage` — reviews, radio interviews,
  reaction-channel creators and collabs are all existing kinds, not new ones.
- Migration 0072 already seeds playlist outreach when a release reaches
  `start_press`.

The five real gaps, in dependency order.

1. **Nothing computes what a show costs.** `estimated_cost_minor` is an input
   nobody fills, and the travel band is four coarse buckets rather than a
   distance. Every "can we take this gig" answer is currently only as good as a
   number a human typed.
2. **Nothing negotiates.** The flow submits an application and waits. There is
   no counter-offer, no target terms, no walk-away floor.
3. **Nothing discovers targets.** `viryaos_outreach_targets` is written only by
   operator upsert through the API. A pitcher over an empty table is a loop over
   zero rows, and this is the single biggest reason playlist pitching would not
   work today.
4. **Spotify and Bandsintown are unfed.** The Phase 1d ingest endpoint exists
   and nothing calls it; Bandsintown tracker counts need an endpoint this
   repository does not call. The agent is blind on both numbers it is being
   asked to grow.
5. **No "do it" button.** The approval queue exists in the API; the control
   plane has no surface for it, and there is no way to say "we did this
   ourselves, mark it done".

Phases renumbered again here. Audit is now 17, control plane 18.

---

## Phase 6 — objectives

An agent without a goal can only react. `viryaos_growth_objectives`: an
operator-declared target on a metric series or first-party outcome, with a
value, a deadline and a scope (workspace, city, event or release).

First targets, matching the stated objective: `spotify/followers`,
`spotify/monthly_listeners` and `bandsintown/trackers`. The Phase 4 queue
becomes objective-aware — an entry contributing to an active objective outranks
one that does not, inserted directly below authority state so a deadline still
wins.

An objective is never evidence of progress. It is a target the measured series
is compared against, and a missed one is reported as missed.

---

## Phase 7 — tour economics (DONE)

Nothing about venue autonomy is safe until the agent can answer "does this gig
pay" from facts rather than from a number somebody typed. This phase is the
prerequisite for Phase 8, and Phase 8 is the prerequisite for ever widening the
`third_party` ceiling on booking.

`crates/crowdrelay-domain/src/tour_economics.rs`, pure and integer-only:

- Operator config, stored once per workspace: home base (Wrocław), vehicle
  profile (count, seats, litres/100 km), fuel price per litre, toll estimate per
  km by country, accommodation rate per night per room, per-diem per person,
  crew size, and the loading/rehearsal overhead that is paid whether or not the
  show happens.
- Input per opportunity: distance in km one way, nights away, border crossings,
  and the offered fee.
- Output: an itemised `ShowCost` — fuel, tolls, accommodation, per diem,
  overhead — and `net_margin_minor = fee - cost`, with the itemisation carried
  so an operator can see *why* a gig was refused rather than a verdict.
- **Vehicle count is derived, not assumed.** Crew plus backline against seats
  decides one car or two, and two cars is roughly double the fuel and tolls —
  which is exactly the case the operator described.
- **Refuses to guess.** Unknown distance, unknown fuel price or unknown nights
  returns `CostEvidence::Insufficient` with the missing field named. A gig whose
  cost cannot be computed is never auto-anything; it is prepared for a human.
  Filling a missing distance with a band average is how an agent talks a band
  into a loss-making 500 km drive.
- Feeds `LiveOpportunitySnapshot.estimated_cost_minor`, so the existing budget,
  score and margin gates keep working unchanged. The coarse `travel_band` stays
  as a secondary signal, not as the cost model.

Merch and bar revenue are **not** modelled. They are real but unpredictable, and
an agent that books a loss-making show because it assumed merch would cover it
is worse than one that refuses.

### As built

`crates/crowdrelay-domain/src/tour_economics.rs` (14 unit tests), migration
`0077_viryaos_tour_economics.sql`, wired into
`load_live_opportunity_snapshots_impl`. `scripts/test_tour_economics_v1.py`,
17 contract tests. `SCHEMA_VERSION` 76 → 77.

The worked example, pinned as a test — five people, a backline that does not fit
in one car, 500 km each way, Polish rates:

| line | value |
|---|---|
| vehicles | 2 (forced by backline, not by seats) |
| round trip | 1 000 km |
| fuel | 1 000 km × 8 l/100 km × 6.50 zł × 2 = 1 040 zł |
| tolls | 240 zł |
| accommodation | 3 rooms × 1 night × 180 zł = 540 zł |
| per diem | 5 people × 2 days × 60 zł = 600 zł |
| overhead | 200 zł |
| **total cost** | **2 620 zł** |
| walk-away fee | cost + 500 zł minimum margin = 3 120 zł |

At a 4 000 zł offer the show clears 1 380 zł. At 1 500 zł it is a loss and the
model says so with the itemisation attached, so the operator sees *why*.

Decisions worth not re-deriving:

- **Vehicle count is `max(ceil(crew/seats), ceil(backline/cargo))`**, capped by
  `max_vehicles`. Both answers are computed and the larger wins: four people
  with a full backline still need two vehicles, six people with no gear also
  need two. Counting only people would have halved the fuel and the tolls on
  exactly the trip this was built for.
- **`costed_from_logistics` is a new field on `LiveOpportunitySnapshot`, and
  `may_auto_submit` requires it.** An uncosted show is still prepared for a
  human — that is what preparing is for — but the band never commits to a long
  drive on a cost nobody computed.
- **An uncosted show scores zero economics points**, rather than defaulting to
  break-even. Otherwise an unknown cost reads as neutral and an uncosted gig
  outranks a costed profitable one.
- **A computed cost overrides the stored `estimated_cost_minor`.** The stored
  figure is still shown when the inputs are missing, but the opportunity is
  marked uncosted, so the typed number can inform a human and can never
  authorise a submission.
- **Zero rates mean "not configured", not "free".** Zero fuel makes every
  distant gig look profitable, which is the precise failure the module exists to
  prevent, so a zero fuel price returns `Insufficient { FuelPrice }`.
- **`nights_away` stated by a promoter beats the overnight threshold.** The
  threshold is operator policy for when nobody said; a stated count is a fact.
- **Money products go through `i128` and saturate**, because distance × rate ×
  vehicles overflows `i64` more easily than it looks in a currency with grosze,
  and an overflow would turn an impossible trip into a bargain.
- `walk_away_fee_minor` is computed here rather than in Phase 8, because the
  floor is arithmetic and the negotiation is policy.

**Not runtime-verified.** No Docker daemon on this machine; migration 0077 has
never run against a real Postgres, and the config still has to be filled with
the band's actual fuel consumption, crew size, backline volume and rates before
any of these numbers mean anything.

---

## Phase 8 — negotiation and booking selectivity

Only after Phase 7, because every rule below needs a computed floor. Three
sub-phases, in this order: knowing how full the year already is, knowing which
shows matter beyond money, and only then talking terms.

### 8a — the year is fuller than the calendar says

`committed_shows_year` counts published events. It does not count the eight
conversations already in progress, so the agent currently believes a year with
ten promising negotiations is empty and keeps finding more. The operator's rule
is the opposite: with ten promising already, find the five best remaining, not
another thirty.

- `pipeline_shows_year`: opportunities in flight for the same calendar year —
  `viryaos_team_opportunities` in `submitted` or `replied`, plus booking targets
  whose newest `viryaos_booking_interactions` disposition is `positive` or
  `booked`. Prepared-but-unsent does **not** count; nothing has been said to
  anybody yet.
- The budget gate reads `committed + pipeline` against `annual_target` and
  `annual_stretch`, not `committed` alone.
- **Scarcity raises the bar rather than closing the door.** Past the annual
  target, the minimum score to even prepare climbs with each slot consumed:
  `effective_minimum = minimum_score + scarcity_step × (committed + pipeline −
  annual_target)`. Ten in the pipeline against a target of fifteen means only
  genuinely strong offers get through, which is precisely "find us the five most
  valuable".
- Counting an unconfirmed pipeline at full weight is deliberate, and it is
  self-correcting: when a negotiation dies the opportunity leaves `submitted`
  or `replied`, the pipeline count drops, and the bar comes back down on the
  next cycle. No decay curve, no guesswork about probability.

### 8b — some shows are worth more than their fee

A Mystic or Pol'and'Rock slot is worth playing at break-even, and the current
score cannot express that: economics is 10 of 100 points and prestige is 0.

- `strategic_value_basis_points` on the opportunity, `0..=10_000`, set by the
  operator or carried in by discovery. Read as three bands for the operator's
  benefit: **Landmark** at 8 500 and above, **Notable** at 6 000, Standard below.
- Score weights rebalanced to make room: fit 30, strategic 25, reputation 15,
  confidence 15, economics 15. Money still counts and counts for more than it
  did; prestige simply counts for more than money at the top of the range.
- **Bounded loss tolerance.** `max_strategic_negative_margin_minor` applies only
  at or above the Landmark floor: a festival slot may run a stated loss, a club
  date on a Tuesday may not. Bounded, never open-ended, and the refusals that
  hold regardless: contract required, exclusivity, past the annual stretch, or a
  cost that could not be computed at all.
- **A Landmark opportunity is never dropped by a budget rule.** It is exempt
  from the scarcity ramp, and at or beyond the annual stretch it is escalated to
  a human rather than silently held. A full year is a reason to ask, not a
  reason for the agent to throw away the best offer of it.
- Where the value comes from: the operator sets it, or discovery matches the
  organiser against a workspace-level list of landmark promoters and festivals.
  A name match is a *suggestion* that an operator confirms, never an automatic
  grant of prestige — "Festival" in a title means nothing.

### 8c — terms

- **Terms ladder** from Phase 7: the walk-away floor is
  `cost + minimum_margin + application_fee`, the target is the fee that makes
  the show clearly worth it, and the opening ask sits above the target. For a
  Landmark show the floor drops by the strategic loss tolerance and nothing
  else changes.
- **State machine** on the opportunity: `proposed → countered → accepted |
  declined | expired`, every transition durable and idempotent, executed through
  the existing outbox.
- **Never, at any autonomy level:** accept below the floor, accept a contract or
  exclusivity, accept when the date is not free, accept past `annual_stretch`,
  accept a stretch show below `stretch_minimum_score_basis_points`, or accept a
  show whose cost is `Insufficient`. These are refusals in the domain, not
  settings an operator can loosen by accident.
- At the current posture every counter and acceptance is `third_party` and
  therefore approval-gated. The agent computes the floor, drafts the counter and
  parks it — and that is already most of the value, because the arithmetic and
  the drafting are the slow parts.
- Widening later changes one ceiling row: the agent may then counter inside a
  pre-approved band and accept only at or above target.

## Phase 9 — target discovery

The pitcher's supply. Without this, Phases 10 and 12 are loops over an empty
table: `viryaos_outreach_targets` is written only by operator upsert today.

### Where curator contacts legitimately come from

There is real free playlist pitching outside Spotify for Artists, and the
routes differ in whether a contact is *published* or *inferred*. Only published
routes are usable.

- **Spotify playlist descriptions.** The Web API returns a playlist's `name`,
  `description`, `owner` and follower count. Curators who accept submissions
  routinely put the route in the description — a form URL, a handle, an address.
  Reading a submission route the curator published for that purpose is the
  intended use of that field, and it is the highest-yield source there is.
  What the API does **not** expose is an owner's email; there is no supported
  way to get one, and there is no inferring it.
- **Curator-run sites and link pages** reachable from the description.
- **Submission platforms**, modelled as channels rather than sources (below).
- **Reply-derived contacts** — anyone who has already written to us.
- **Operator import** — a CSV or Sheet of contacts the band already has.
- **Scene-adjacent playlists**: owners of playlists that already contain
  comparable artists, which is both a fit signal and a contact route.

### The rule that keeps this clean

**A candidate is not a target.** Candidates arrive unverified, carrying their
source and the raw evidence the route was extracted from — the description
snippet, the page, the reply. They become targets only when the route is
confirmed. Extraction is strict: an explicit submission intent, an explicit
address or URL. **Never infer a contact**, never guess an address pattern from a
name and a domain, never take a personal address that was not offered for
submissions. A contact the agent invented is a bounce at best and a burned
relationship at worst, and burned curators do not come back.

Never fetch anything a platform's terms forbid, and record the source on every
candidate so a bad source can be revoked wholesale later.

### Free is enforced, not assumed

Submission platforms differ: some are free, several sell credits, and a few sell
placement. So a **submission channel** carries its own cost, and the cost decides
the class:

- free channel → the pitch is `third_party` and follows the normal ceiling
- credit or fee channel → the pitch is `paid`, and is therefore gated by the
  spend ceiling no matter how small the fee

That makes "free only" a property the system enforces rather than a habit an
operator has to remember. **Paid-placement services are refused outright**, at
every autonomy level and every ceiling: buying a placement is fabricated
engagement wearing a suit, and it gets the artist flagged.

### Guarding against the other direction

Playlist pitching has a large scam surface. Candidates are screened before they
are ever pitched: implausible follower-to-engagement ratios, placement-for-sale
language in the description, and playlists whose track list churns
indiscriminately. A refusal is recorded with its reason, so the same bad
candidate is not rediscovered every week.

### Ingestion

`POST /v1/admin/autopilot/outreach/candidates` — idempotent, replay-safe,
requires `Idempotency-Key`, accepts a bounded batch. n8n calls Spotify and the
directories and posts candidates in; CrowdRelay stays the authority for
candidates, targets, screening and policy. No OAuth flow moves into CrowdRelay
for this — discovery needs only an app token, and the adapter already holds one.

Deduplicate on contact identity, and carry a per-kind fit score so a metal
reaction channel is never pitched a folk single.

### As built — 2026-08-23

Migration `0082_viryaos_target_discovery.sql`, `crowdrelay_domain::target_discovery`,
and four admin routes under `/v1/admin/autopilot/outreach/`.

- **Screening happens on write, not in a sweep.** A candidate arrives, is judged
  and is stored with its verdict, so a refusal is durable and the same bad
  candidate is never rediscovered, re-screened and re-refused next week. That is
  what makes running discovery often cheap.
- **Refusal reasons are a closed set**: `route_inferred`, `evidence_missing`,
  `paid_placement`, `sells_placement`, `implausible_engagement`,
  `indiscriminate_churn`, `poor_fit`, `too_small`. The first two and
  `paid_placement` are permanent regardless of policy; the rest move with the
  thresholds in `TargetDiscoveryPolicy`.
- **Dedupe is on contact identity**, `(workspace, route_kind, route_value)`, so
  finding the same curator through a second source is a duplicate rather than a
  second candidate.
- **Only an email route promotes.** A form or a handle is a real published route
  with no pitcher yet, so it stays an admitted candidate rather than becoming a
  target row with nowhere to put the address. Phase 10 and 12 change that.
- **Promotion never resets a relationship**: an address the band already holds
  keeps its score, history and do-not-contact flag, and only gains the
  provenance link back to the candidate.
- Screening thresholds are `TargetDiscoveryPolicy::default()` today. Making them
  operator-editable is a `viryaos_autopilot_policies` row and a context, not a
  new subsystem.

Proven end to end against Postgres 18 in
`crates/crowdrelay-infra/tests/autopilot_target_discovery_postgres.rs`.

### The half that was missing — added 2026-08-23

Everything above is inbound. Nothing decided to *go looking*, so an empty
`viryaos_outreach_targets` was a stable state rather than a problem the agent
could see, and in production it stayed at zero rows while `outreach.send` sat
advertised and idle. `outreach_supply` (migration `0083`) is the context that
notices the floor and asks for a sweep; see the production audit below for what
it does and what it deliberately refuses to do.

## Phase 10 — free-reach pitcher

Reviews, radio interviews, reaction-channel creators, collabs, media patronage.
All free, all `third_party`, all approval-gated at the current posture.

- Runs as **waves**, not one-offs: a ranked batch per kind per release or per
  tour leg, sized to the weekly third-party budget from Phase 5b.
- Each pitch carries an **evidence packet** assembled from real first-party
  data — recent series movement, city signals, real ticket and attendance
  numbers, existing coverage. No adjectives the numbers do not support.
- Follow-up discipline is already in `OutreachPolicy`: `initial_cooldown_days`,
  `followup_after_days`, `maximum_followups`, `declined_cooldown_days`. Reuse
  it; do not invent a second cadence.
- A wave is presented for one-click approval as a wave. Approving forty pitches
  individually is how a human stops approving.

---

## Phase 11 — Spotify and Bandsintown feeds

The agent cannot grow what it cannot see, and it currently sees neither.

- **Bandsintown trackers.** `event_sync/bandsintown.rs` calls only
  `/artists/{artist}/events`, whose response carries no tracker count. Add the
  `/artists/{artist}` call, confirm field semantics against a real response, and
  feed `bandsintown/trackers` through the Phase 1d ingest path. Phase 2 refused
  to invent this series; this retires that refusal properly.
- **Spotify followers and monthly listeners** through the existing n8n adapters
  into the same ingest endpoint. CrowdRelay does not grow an OAuth flow for
  Spotify until there is a reason it must own the credential.
- Do this **before** enabling any play that pushes on these numbers. Pushing on
  an unobservable metric is indistinguishable from doing nothing.

---

### As built — 2026-08-23 (the seeing half only)

`GET /v1/admin/autopilot/growth-metrics/coverage` answers "what can the agent
see?" for `spotify`, `youtube`, `bandsintown` and `social`, reporting `missing`,
`stale` or `live` per platform. A platform nobody connected reads as `missing`
rather than as an empty list, because silence from an unconnected platform is
not evidence that nothing is happening there.

The schema already accepted these platforms, and ingestion is the ordinary
series/points path, so what is left of Phase 11 is entirely adapter work in n8n:
declare each series once with an honest `expected_interval_hours`, then post
absolute values. The contract is written up in `n8n/viryaos-executor-contract.md`.
Until an adapter runs, coverage will honestly report four missing feeds.

## Phase 12 — playlist pitcher

Two different things that are usually confused, and the difference decides what
can be automated.

- **Spotify editorial pitch is one form per release inside Spotify for Artists,
  with no API.** The agent cannot submit it and must not pretend to. What it
  *can* do: detect the release, compute the deadline (pitch before the release
  is delivered), assemble the pitch text and evidence, park it as a human task
  with a hard due date, and escalate as the deadline approaches. That is most of
  the work and all of the discipline.
- **Everything else is the free route, and it is the bigger half.** Independent
  curators reached through the submission route they published, free submission
  platforms, and scene-adjacent playlist owners — all run as Phase 10 waves with
  fit-ranked targets, evidence packets, cadence, follow-ups and decline
  cooldowns.
- **What we offer, since it is not money.** The track and its assets, a real
  reason this playlist specifically, the numbers that support it, and a promise
  we can keep — we push placements to our own audience, which is worth something
  to a curator building one. Offering anything reciprocal-for-placement is
  refused: that is a paid placement with extra steps.
- **Never pay for placement and never route through a service that sells it.**
  Paid or reciprocal placement is fabricated engagement wearing a suit; it also
  gets the artist flagged.

### The conversation does not end at "sent"

A pitcher that only counts sends is a spam cannon with a dashboard. Every pitch
carries through to an outcome, and the outcomes are the existing
`OutreachReplyDisposition` values plus the ones a playlist needs:

`sent → replied | bounced | silent` and then
`placed | declined | ghosted | withdrawn`.

- **Silent is not declined.** A curator who never answered is followed up
  according to `OutreachPolicy` and then goes quiet, not onto a blacklist.
- **Ghosted** — replied positively, never placed — is tracked separately from
  declined, because it predicts differently next release.
- **Withdrawn** — placed and then removed inside the verification window — is
  the single strongest scam signal there is.

### Placement is verified, never taken on the curator's word

This is the anti-scam core, and it is cheap because the data is public.

- On a claimed placement, confirm the track is actually in that playlist through
  the Spotify Web API. A claim without a confirmation is recorded as `ghosted`,
  never as a placement.
- Re-check at +7 and +30 days. Playlists that add a track for a screenshot and
  remove it days later are a known pattern; `withdrawn` catches exactly that.
- A curator with a `withdrawn` outcome is suppressed permanently and their other
  playlists are re-screened, because the behaviour belongs to the operator, not
  to the playlist.
- **A placement that cannot be verified never counts toward a result.** The
  measured record has to be the thing that actually happened, or the learning in
  Phase 15 is trained on somebody else's marketing.

### Screening, stated concretely

Screened before a candidate is ever pitched, refusal recorded with its reason so
the same one is not rediscovered weekly:

- follower count wildly out of line with saves and with track count — the
  signature of a bought audience
- payment, credits or "guaranteed placement" language in the description
- a demand for a follow, a stream or a reciprocal add in exchange
- thousands of tracks with near-total churn, where nothing stays
- one operator behind many playlists that all show the same pattern
- a submission route that leads to a paid platform we already classify as `paid`

### Suppression is permanent where it should be

- Hard bounce: the address is wrong. Target deactivated, not retried.
- "Do not contact": permanent, never expires, survives re-discovery — a
  re-imported candidate matching a suppressed identity stays suppressed.
- Declined: `declined_cooldown_days` from `OutreachPolicy`, then eligible again
  for a *different* release. Never the same track twice.
- Identity is matched across playlists, so a curator who declined once is not
  pitched again the same week through a second playlist they own.

### Sending like somebody who wants replies

Deliverability is not a detail; a burned sending domain ends the whole channel.

- Volume ramps rather than starting at the weekly cap, and the Phase 5b budget
  is the ceiling, not the target.
- Bounces and complaints are ingested and acted on. A rising bounce rate stops
  the wave rather than being reported after it.
- Every pitch identifies the sender, says where the contact came from, and
  carries a working opt-out. Under GDPR, contacting a business address the
  curator published for submissions is defensible; hiding who is asking is not.
- No misleading subject lines, no fake threading, no "re:" on a first contact.

### What counts as a result

Reported separately, never added together:

- **First-party and attributable**: pitches sent, replies, verified placements,
  smart-link clicks.
- **Correlational**: follower, save and monthly-listener movement after a wave.
  Reported as correlational with the window stated, because a playlist add and a
  release week and a show announcement all land in the same fortnight.

The honest summary an operator gets is "eleven pitches, three replies, one
verified placement, followers up 240 in the following fortnight — the last
number is not attributed to the first three."

## Phase 13 — plays

The agent's unit of work: a typed, stateful, multi-step campaign with a
hypothesis, entry conditions, ordered steps each carrying its own `ActionClass`,
timing relative to an anchor, a stop rule and a success metric.

The design point that makes safest-autonomy workable: a play executes its
`owned_audience` and `first_party_reversible` steps autonomously and parks its
`third_party` steps for approval **without blocking**. A gated step unapproved
past its window is skipped and recorded as skipped, never silently dropped.

State in one table with a state machine; steps execute through the existing
outbox. No new scheduler, no new broker.

### Play library v1

1. **Follow-ask ladder** — owned audience, autonomous. Engaged fans with no
   follow-ask in the cooldown get one message with exactly one call to action
   through a tracked smart link.
2. **Track-us ask** — owned audience, autonomous. City-scoped, timed to announce
   and post-show. The most under-used free lever the band has.
3. **Release runway** — mixed. Pre-save link and landing surface, owned-audience
   announce, curator wave queued for approval, release-day push, sustain ask.
4. **Curator and playlist waves** — Phase 12.
5. **Free-reach waves** — Phase 10.
6. **Listing completeness sweep** — first party, autonomous. Every published
   upcoming event carries a complete Bandsintown listing with a tracked link.
7. **Dormant revival** — owned audience, autonomous.

---

## Phase 14 — measurement and honest attribution

The unit of measurement is the play.

- Extend `viryaos_autopilot_measurements.measurement_kind` with growth-metric
  outcome kinds, following migration 0049's pattern.
- A play's effect is its success metric's velocity over the play window against
  its own pre-play baseline, recorded as `improved`/`neutral`/`worsened` in
  `viryaos_autopilot_outcomes` — the columns already exist.
- **Two claims, never conflated.** A smart-link click is first-party attribution
  and is reported as attribution. A follower or tracker series moving after a
  send is correlational and is reported as correlational. The API says which on
  every number it returns.
- Show economics close the loop the same way: predicted cost against settled
  cost, so the Phase 7 model is corrected by reality rather than trusted.
- Where a join key is missing, return `evidence: insufficient` with the reason.
  Never interpolate a path, never fill a gap with a plausible number.

---

## Phase 15 — learning

Play and pitch selection weights move with the measured record; a play that
repeatedly measures `worsened` is proposed less and eventually retires itself.
Bounded, explainable, stored as data rather than as a model. **Authority never
widens automatically** — neither the context ladder nor the class ceiling moves
without a human, however good the record looks.

---

## Phase 16 — operator brief

Extend `load_chief_of_staff`: what the agent did alone, what it is about to do,
what is parked for approval, what it stopped and why, what moved. Delivery
through the existing outbox → n8n path.

---

## Phase 17 — audit

Five passes, written up here with findings and resolution: correctness,
usefulness against real VIRYA data, feature completeness, **safety** (every
ceiling held, no cap exceeded, no cooldown breached, kill switch effective, no
fabricated engagement, no gig accepted below floor), and performance plus code
quality.

---

## Phase 18 — control plane: find, then "do it"

The operator's stated shape for the no-autonomy mode, and the reason it is last
rather than optional.

In `crowdrelay-control-plane` (operator plane, never tenant-critical):

- **The opportunity board.** Everything the agent found and parked — gigs with
  their computed economics and the counter it would send, free-reach pitches,
  playlist waves, editorial pitch deadlines — each with its evidence and the
  consequence of ignoring it.
- **"Do it"** approves the parked action through the existing approval endpoint.
  One button, one action, no new authority path.
- **"Done ourselves"** records that a human handled it outside the system, so
  the agent stops proposing it and the measured record stays honest. This is a
  first-class outcome, not a dismissal — an opportunity a human took is a
  success, and recording it as ignored would teach the ranker the wrong thing.
- Read-only over CrowdRelay's API contract otherwise. The control plane must not
  become a second source of truth, and no business policy moves into it.

---

## Implementation order and state, at a glance

Kept here so a session that starts cold knows what is real, what is planned and
what depends on what. Phases 1 to 7 are code; 8 onward is plan.

| Phase | What it is | State |
|---|---|---|
| 1 | Metric series, trend and anomaly detector | DONE |
| 2 | First-party metric sources | DONE |
| 3 | Growth-debt detector | DONE, one kind blocked |
| 4 | Ranked cross-context queue | DONE |
| 5 | Autonomy envelope: class ceiling, budgets, kill switch, dry run | DONE |
| 6 | Objectives | plan |
| 7 | Tour economics | DONE |
| 8 | Booking selectivity and negotiation | plan |
| 9 | Target discovery | DONE (ingestion, screening, promotion, **and the request that fills it**) |
| 10 | Free-reach pitcher | plan |
| 11 | Spotify and Bandsintown feeds | partial: coverage is visible, adapters unwritten |
| 12 | Playlist pitcher | plan |
| 13 | Plays | plan |
| 14 | Measurement and honest attribution | plan |
| 15 | Learning | plan |
| 16 | Operator brief | plan |
| 17 | Audit | plan |
| 18 | Control plane: find, then "do it" | plan |
| 19 | Hunt for more free autonomous work | plan |
| 20 | Layering, hardening, performance, completeness | plan |

Hard dependencies, the ones that make a phase pointless if taken out of order:

- **7 before 8.** Negotiating without a computed floor is guessing with the
  band's money.
- **9 before 10 and 12.** A pitcher with no targets is a loop over zero rows.
- **11 before 13's Spotify and Bandsintown plays.** Pushing on a number the
  agent cannot see is indistinguishable from doing nothing.
- **5 before anything acts.** Already true; do not undo it.
- **14 before 15.** Learning from unverified outcomes trains the ranker on
  somebody else's marketing.

Everything else can move. 6 is small and unblocks ranking by objective; 16 is
worth pulling forward the moment the agent does anything unattended, because an
operator who cannot see what it did will switch it off.

### Still open, needing the operator rather than the code

- **Tour config is seeded with declared figures** — 200 zł per 100 km round trip
  for two cars, 8 l/100 km at 8 zł as the fallback, six crew, 300 zł overhead,
  500 zł minimum margin, 180 zł a room, 60 zł per diem. Editable through
  `GET`/`PUT /v1/admin/autopilot/tour-economics`. **Three values are still
  guesses and change the answer**: backline volume (1 200 l) and vehicle cargo
  capacity (900 l) together decide whether a trip needs one car or two, which
  moves every downstream number; the 350 km overnight threshold decides whether
  a trip books beds. Confirm all three before trusting a verdict.
- **`StaleContactData` growth-debt kind stays blocked** — the schema has no
  verification timestamp, and `updated_at` is not one. Phase 3's open question.
- **The schema now runs against a real Postgres 18** — 2026-08-23, every
  migration through 0081 applied by the runner against a local disposable
  database, and the gated-claim behaviour is covered by
  `crates/crowdrelay-infra/tests/autopilot_gated_claim_postgres.rs`, which is
  run with `CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL`. The numbers the agent
  reports are still unverified against production volumes.

---

---

## Production audit — 2026-08-23, against the deployed agent

Taken from the production database and executor registry at commit `d42ace9`,
not from this file. The roadmap above says what was built; this section says
what is *running*, which turned out to be a different question.

### The three post-deploy checks

All three pass on the deployed build, over roughly three autopilot cycles
(poll interval 300 s):

- **`awaiting_executor` = 0**, and parked capabilities warn once per capability
  per cycle rather than once per action — two "autopilot actions are parked"
  lines over three cycles, four team-handoff lines over sixteen.
- **Content-source versions stopped moving.** Frozen since the deploy itself;
  the runaway that took one source from version 1 to 283 in thirteen days is
  gone.
- **No new `state_changed`.** The last one is timestamped before the deploy.
  The twenty-four failed `content.artifact.request` actions all predate it.

### What is actually running

| Capability | State | Evidence from production |
|---|---|---|
| Autonomy envelope | **DISABLED** | `agent_enabled=false`, `dry_run=true` |
| Metric series, trend, anomaly | REAL | 5 series, 60 points, current to the hour |
| First-party metric sources | REAL | `active_fans`, `activated_fans_30d`, `paid_tickets`, `paid_buyers`, `paid_orders` |
| Spotify / YouTube / Bandsintown metrics | **EMPTY** | 0 series; only the *coverage* report is real |
| Meta / Instagram metrics | **PLAN ONLY** | `MetricPlatform::Social` exists; nothing writes it |
| Growth-debt detectors | REAL, one kind blocked | `StaleContactData` still has no clock |
| Next Best Action queue | REAL | route and reads live |
| Target discovery | REAL code, **EMPTY data** | 0 candidates, 0 channels, 0 targets |
| Outreach send | REAL executor, **EMPTY supply** | `outreach.send` advertised and idle |
| Booking outreach | REAL executor, **EMPTY supply** | `booking.outreach` advertised, 0 booking targets |
| Tour economics | REAL, three inputs guessed | unchanged from Phase 7 |
| Booking negotiation | PLAN ONLY | — |
| Free-reach pitcher, playlist pitcher | PLAN ONLY | — |
| Press / reviews / interviews | **PARTIAL**, effectively empty | routes exist; 3 beacons total |
| Fan growth (`show_growth`) | **DISABLED** | `show.growth` not advertised; 3 failed actions |
| Content supply | **DISABLED** | `content.artifact` not advertised; 4 parked |
| Beacon discovery / outreach | **DISABLED** | `beacon.*` not advertised |
| Experiments | REAL code, EMPTY | 0 experiments |
| Measurement and attribution | PLAN ONLY | — |
| Learning | PLAN ONLY | — |
| Operator brief | **PARTIAL** | `chief-of-staff` is a read endpoint; nothing is ever sent |

### The biggest blocker, stated precisely

It is not the envelope. The envelope is a switch, and flipping it today would
change nothing, which is the actual finding.

**Every live execution path starves, and every path with something to work on
is gated.** The executor advertises six capabilities:
`fan.lifecycle.message`, `booking.outreach`, `outreach.send`, `funding.submit`,
`opportunity.application`, `ops.alert`. Of those, the two that grow anything —
`outreach.send` and `booking.outreach` — read from tables holding **zero rows**.
Meanwhile `content.artifact` (5 content sources), `show.growth` (5 events) and
`beacon.*` (3 beacons) have subjects to act on and are not advertised at all.

Underneath that sits one thing that is squarely CrowdRelay's problem rather
than n8n's: **the agent could not ask for supply.** Discovery was inbound only,
so zero targets was a stable state rather than something the agent could
notice. A brain that cannot say "I have nothing to work with" is not blocked on
autonomy; it is blocked on perception.

### Smallest safe implementation — DONE 2026-08-23

`outreach_supply`, the twentieth context. Migration `0083`,
`crowdrelay_domain::target_discovery::evaluate_outreach_supply`,
`AutopilotActionPayload::RequestOutreachDiscovery`, capability
`outreach.discovery`.

- **`first_party_reversible`.** It reads published data, contacts nobody and
  buys nothing, so it needs no new autonomy and spends no outreach budget.
  Every judgement about who may be contacted stays in screening.
- **It holds when the queue is waiting on a human.** Admitted candidates above
  the floor produce `AwaitingRouteConfirmation`, not another sweep. Fetching
  more supply while an unworked queue is full is how an autonomous system feels
  busy and changes nothing.
- **It stops after three barren sweeps.** Counted as a *run ending at the most
  recent sweep*, not a total, and a sweep nobody answered breaks the run rather
  than extending it — otherwise one broken workflow disables discovery
  permanently.
- **Quota 2/day, cooldown 24 h, provisioned disabled at `observe`.**

Proven against Postgres 18 in
`crates/crowdrelay-infra/tests/autopilot_outreach_supply_postgres.rs`: the
per-sweep window, the barren run, the unanswered sweep, and the fact that
do-not-contact and inactive targets are not supply.

### What is still missing, in the order that unblocks the most

1. **The executor must advertise `outreach.discovery`** and run a sweep. Until
   it does, the new context emits an action that parks — visible, correct and
   still zero targets. This is now a workflow task, not a code task.
2. **Advertise `content.artifact`, `show.growth` and `beacon.*`.** Three
   capabilities with real subjects waiting, gated by heartbeat rather than by
   policy. This is the cheapest growth available and needs no Rust at all.
3. **Off-platform metric adapters** (Phase 11). The agent reports honestly that
   it cannot see Spotify, YouTube or Bandsintown, which is better than guessing
   and still means it cannot tell whether anything worked.
4. **Turn the envelope on** — after 1 and 2, not before. Enabling an agent whose
   every path is empty proves nothing and teaches the operator to distrust it.
5. **Phase 14 (verification) then 15 (learning).** Both are still plan, and
   ranking without verified outcomes trains on somebody else's marketing.
6. **Phase 16 (operator brief).** `chief-of-staff` already computes it; nothing
   delivers it. Worth pulling forward the moment step 4 happens.

### Next measurable growth loop

`outreach_supply` notices the floor → adapter sweeps published sources →
candidates screened on write → operator confirms routes → `outreach.send`
pitches confirmed targets → replies recorded on the target → supply and reply
rate become the first honest measure of whether any of it works.

The loop is closed in CrowdRelay end to end today except for the sweep itself
and the pitcher, and the count to watch is the one that has never moved:
`viryaos_outreach_targets`.

---

## Phase 19 — hunt for more free autonomous work

The point of the whole system is a brain that does useful things and gives the
band its time back. Everything above was specified from what the operator asked
for; this phase asks the opposite question — what else is the agent already able
to do for free that nobody has thought to ask for?

Run as a survey, not a build: enumerate, score, then implement only what earns
it. For every candidate, three questions decide it.

1. **Does it save real time, or does it just look busy?** An action that
   produces a list somebody still has to read has saved nothing.
2. **Is it free and reversible enough to run unattended**, under the Phase 5
   class ceiling as it stands?
3. **Is there a first-party signal that it worked?** If not, it cannot be
   measured, cannot be learned from, and will quietly rot.

Candidate ground already visible in the repository, to be assessed rather than
assumed:

- **Post-show follow-through.** Attendance, merch and beacon data all exist the
  morning after; the thank-you, the setlist post, the next-city ask and the
  merch window are all owned-audience and all currently manual.
- **Calendar and routing hygiene.** Two confirmed shows 600 km apart on
  consecutive days is a fact the system can see and nobody enjoys discovering
  late. Phase 7's cost model already knows what the drive costs.
- **Release-asset completeness.** A release with no smart link, no pre-save
  surface or an incomplete listing is first-party debt the agent can simply fix.
- **Contact hygiene at the source.** Bounces, dead links and stale routes
  discovered during a wave, repaired rather than reported.
- **Ticket-sale watch.** A show selling far below its own history at T-14 is
  visible now; the response is owned-audience and free.
- **Fan-milestone moments.** First ticket, tenth show, referral that converted —
  real reasons to say something true, and the strongest owned-audience material
  there is.
- **Reply triage.** Inbound replies classified and routed, so a human reads the
  three that need a human rather than forty that do not.

Output is a written scoring of every candidate in this file, with the rejected
ones and why — a rejected idea with a reason is worth as much next time as an
accepted one.

---

## Phase 20 — final pass: layering, hardening, performance, completeness

Last, deliberately, because doing it earlier means doing it twice.

- **Domain-driven layering.** Ubiquitous language consistent across domain,
  application and API; no anaemic types carrying logic that belongs in the
  domain; aggregate boundaries honest about what a single transaction may
  change. `crowdrelay-application` still holding zero sqlx call sites, and no
  writes in `crowdrelay-api`.
- **Hardening.** Every new endpoint replay-safe and bounded; every new snapshot
  loader workspace-scoped with no cross-tenant path; clock skew, backfills and
  out-of-order delivery covered on every rule added since Phase 1; the autonomy
  ceiling and envelope re-verified end to end against a live database rather
  than against a contract test reading source.
- **Performance.** Every query added since Phase 1 measured under realistic row
  counts, index coverage confirmed, no per-subject N+1, response sizes bounded,
  and the Autopilot cycle measured as a whole — the agent runs on a 12 GB box
  that also serves production.
- **Completeness.** Every requirement in this file accounted for, each either
  built, explicitly deferred with a reason, or explicitly refused with a reason.
  A requirement that quietly vanished is the failure mode this pass exists to
  catch.
- **Sanity.** Read the whole thing as an operator would: switch the agent on in
  dry run against real VIRYA data and read every action it proposes for a week.
  Anything that would have embarrassed the band is a bug, whatever the tests say.

---

## Acquisition — measured 2026-08-23, before the Google Play launch

The plan above assumes an audience to activate. Counted against production on
the day of the app launch, the reachable universe is:

| source | count |
|---|---|
| fans | 19 total, 8 active, 19 with marketing consent |
| beacons (latarniks) | 3 |
| booking targets | 0 |
| outreach targets | 0 |
| distinct ticket buyers | 0 |
| event interests | 5 |

**Nineteen people.** This is the single most important number in this document
and it reorders everything below it.

### What follows from that

- **Do not automate outreach at this scale.** Nineteen personal messages
  written by a human on a Sunday afternoon will convert better than any campaign
  system, and cost less to build. The owned-audience machinery already
  shipped — post-show follow ask, milestones, dormant revival — is correct and
  should stay switched off until there is a list worth running it on.
- **The machinery is not the gap.** Beacon invites (`create_invite`,
  `create_invite_batch`, delivery jobs, `exchange_invite`), fan signup
  (`POST /v1/fans`), tracked links (`/v1/go/{slug}`), concert QR and referral
  codes all exist and work. There is simply nobody in the tables to send to.
- **The band's real audience is off-platform.** It is on Spotify, on
  Bandsintown, on socials, and in the room at shows. CrowdRelay holds none of
  it. Acquisition here means *migrating* an audience the band already has into
  one it owns — and every lever for that starts outside this system.

### The threshold, stated so it is not argued about later

Automated owned-audience campaigns earn their place at roughly **500 consented
fans**. Below that a human wins on quality and costs nothing to run. At 19, the
agent's job is not to message anybody: it is to make sure that every person who
arrives from the launch is captured, attributed and reachable next time.

### What the agent should actually do for the launch

Ordered by what it can do alone, today, for free.

1. **Make every launch channel a tracked link.** One smart link per place the
   band posts — Play listing, Spotify bio, Bandsintown, each social account,
   each personal profile. Without this, launch day produces installs nobody can
   attribute and the next campaign is planned blind. The link machinery shipped;
   the links themselves do not exist yet.
2. **Give the nineteen a referral code each.** At this size every existing fan
   is a door, and a referral that converts is the only growth loop that
   compounds without a budget. The referral ledger already exists.
3. **Concert QR at the merch table for every show.** The room is the highest-
   intent audience the band will ever stand in front of, and capture there costs
   a printed square. `concert_qr` campaigns already exist and none are running.
4. **Capture, then ask.** A fan captured on launch day with consent is worth
   more than ten invitations sent to people who never asked.

### The latarnik campaign, honestly

Recruiting beacons by email needs candidate beacons, and there are three. The
sequence is discovery first, invitation second — Phase 9 exists for exactly
this, and running an invitation campaign before it is running a campaign to an
empty list. Beacon discovery (`RequestBeaconDiscovery`, first-party and free)
is the piece that fills the table; the invite batch endpoint is already waiting
for it.

### What this does not change

Everything built so far stays. The measurement, the ceilings, the envelope, the
cost model and the levers are all correct and all cheap to keep. They are
simply premature to *switch on*, and the honest thing is to say so rather than
to report a working growth agent that has nobody to grow.

---

## Reaching a thousand — the acquisition plan

Target: 1 000 real, consented people. Channels: social, YouTube, Google search
and LinkedIn, the last by announcing the system itself as a piece of work worth
looking at.

### Two funnels that must never be added together

They look like one number and they are not.

- **Music funnel** — YouTube, Spotify, Bandsintown, socials, the room at a
  show. Produces people who might come to a gig and buy a shirt.
- **Tech funnel** — LinkedIn, Google search, developer communities. Produces
  people interested in how the thing is built. Some will install the app out of
  curiosity, a few will become collaborators or employers, and **almost none
  will become metal fans.**

A person acquired from a LinkedIn post about Rust architecture is not a fan, and
counting them as one makes every downstream metric lie. Every acquisition
carries its funnel, and the read models report the two separately, always. The
temptation to merge them will be strongest exactly when one is doing well.

### The arithmetic, before the optimism

Honest ranges rather than a plan that assumes everything lands:

- LinkedIn suppresses outbound links, so click-through to an external URL runs
  around **1–2%** of impressions.
- A strong post from a small account reaches roughly **3 000–20 000**
  impressions; a genuine breakout can do far more, but cannot be planned for.
- A landing page that is actually interesting converts **5–15%** of arrivals
  into a consented signup.

Multiply that through: 10 000 impressions is 100–200 clicks is **5–30 signups**.

**One thousand consented people is therefore not one announcement.** It is
either a sustained series over months, or one breakout plus capture good enough
to keep the traffic, or several channels compounding. Any plan that promises
1 000 from a single post is wrong, and building for it would mean building for a
number that never arrives. Plan for a series and be delighted by a breakout.

### What must exist before the announcement goes out

In order, because each one is wasted without the one before it.

1. **A tracked link per channel, each with its own campaign.** This is not
   measurement hygiene, it is the *only* mechanism by which source attribution
   works: `FanSignupInput` carries no source field, so a fan's channel is
   derived from the campaign behind the smart link they arrived through.
   Without per-channel links, launch traffic is unattributable and the second
   post is planned blind. The link machinery shipped; the links do not exist.
2. **A landing surface that converts.** Lives in `virya`, not here. A technical
   audience arriving from LinkedIn and a fan arriving from Bandsintown want
   different pages, and one page will lose both.
3. **The code has to survive being looked at.** The LinkedIn pitch is "this is a
   good piece of engineering", and the first thing a technical reader does is
   open the repository. Comments that read as machine-written undercut that
   claim in seconds — this is a prerequisite for the announcement, not a
   cosmetic cleanup, and it is the one item on this list that is genuinely
   urgent before posting.

### What the agent does, and what it must not

**Does:** creates the per-channel links, records every click, captures every
signup with its source, attributes referrals, and reports each funnel
separately and honestly — including saying "insufficient evidence" when a click
cannot be tied to a signup.

**Does not:** write the posts. A LinkedIn announcement about a system this
opinionated has to sound like the person who built it, and a generated one will
read like every other generated one. This is the highest-leverage hour the band
spends and it is not automatable.

### Sequencing against the 19 already here

The owned-audience machinery stays off until roughly 500 consented fans. Until
then acquisition is the only thing that matters, and the agent's contribution is
capture and attribution rather than outreach. The moment the list crosses the
threshold, everything already built — milestones, post-show follow asks, dormant
revival, the follow ladder — switches on against an audience worth running it
on. That is the order: get the people, then be worth their attention.

---

## Snowballing — the loop, and the order to build it in

The sequence is right: get ready, finish the agent, then run the campaign. One
sharpening, because it changes what "finish the agent" means.

**Finish the capture and intelligence half. Leave the outreach half switched
off.** Those are two different halves of the same agent, and only one of them is
useful before the campaign. Outreach machinery aimed at nineteen people is the
system nobody can afford; capture and intelligence aimed at a thousand arrivals
is the difference between a campaign that compounds and one that produces a
spike and nothing after it.

### The loop that actually snowballs

It is already closed in the schema, which is why it is worth naming rather than
inventing something new:

```
fan signs up with a city
      -> city_signal_fans counts real people per city
      -> viryaos_city_market_signals turns that into live_demand evidence
      -> booking_opportunity picks the city that is actually warm
      -> a show gets played there
      -> concert QR captures the room
      -> more fans in that city
      -> stronger signal
```

Every arrow exists. A fan signup already requires a city, `city_signal_fans`
already counts distinct fans per city, and the booking rule already reads city
opportunity. Nothing needs building for the loop to turn — it needs **people
entering at the top and being captured at the bottom.**

This is why Polish metalheads specifically is the right target and not a
limitation: the loop is geographic. A thousand fans scattered across Europe
produce no bookable city; two hundred in Wrocław, Kraków, Poznań and Warszawa
produce four shows, and four shows captured properly produce the next four.

### Scene nodes are the multiplier

For a Polish metal scene the amplifier is not advertising, it is the people who
already convene metalheads: venues, local promoters, zines, radio shows, other
bands, Discord and Facebook groups. That is exactly what a beacon is, and there
are **three**. Beacon discovery (`RequestBeaconDiscovery`, first-party and free)
is the single highest-leverage autonomous action available, because one scene
node reaches a room the band cannot reach alone and the invite machinery behind
it is already built and idle.

### Ready for snowballing means these five things and no more

1. **Per-channel tracked links with campaigns**, so every arrival carries a
   source. Without this the campaign teaches nothing.
2. **City on every signup**, which the API already enforces — verify it survives
   the landing page rather than defaulting to one city.
3. **A referral code per fan**, so each person is a door. The ledger exists and
   the codes do not.
4. **Concert QR live for every show**, so the room is captured. Campaigns exist
   and none are running.
5. **Beacon discovery running**, filling the latarnik table the invite endpoints
   are waiting for.

Nothing else is required before the campaign, and adding more would be
building ahead of evidence again.

### Then the campaign, and what it must return

A real campaign for a thousand Polish metalheads, run across the music funnel
with the tech funnel kept separate and separately counted. What it has to give
back, beyond the people:

- which channel produced fans who **stayed**, not just fans who signed up
- which **cities** crossed the threshold where a show becomes bookable
- which **scene nodes** produced more than they cost to approach
- and where the evidence is too thin to say — reported as thin rather than
  rounded up

That intelligence is what makes the second thousand cheaper than the first. A
campaign that delivers a thousand people and no answer to those four questions
has bought a number instead of a position.

---

## Campaign brief — 1 000 active Polish metalheads (recorded 2026-08-23, not yet started)

Operator's brief, kept verbatim in substance so a later session does not
reinterpret it, with the engineering gaps named underneath.

**Goal.** 1 000 real *active* users in Poland. Not followers, impressions or
signups. Active = signup + consent + at least one meaningful action within 30
days. Meaningful: content, music, event/RSVP, referral, merch, Signal.

**Budget.** 0 PLN paid acquisition. Existing audiences and organic distribution
only: FB groups, Reddit, Discord, bands, promoters, venues, festivals,
playlists, metal media, IG/TikTok/YouTube, referrals.

**Core strategy.** Do not sell "follow Virya". Build something metalheads want
whether or not they have heard of the band — Polish metal discovery, curated
tracks, exclusive live/demo/stems, a useful scene resource. Flow: distribution,
useful offer, low-friction signup, immediate value, meaningful action, referral.

**Referral is a product feature, not a hope.** Join, get exclusive value, invite
two metalheads, unlock more. Measure activated referrals; never assume virality.

**Experiment discipline.** Test each channel separately. First milestone 100
activated users in 7 days at 0 PLN, then 100 → 250 → 500 → 1 000. Kill losers,
scale repeatable winners. Operating loop: hypothesis, small test, measure,
kill or scale, repeat.

**Working funnel hypothesis, explicitly not a forecast.** 10k–30k qualified
visits → 3k–5k signups → 1.5k–2k activated → 1k active at 30 days. Replace every
number with real data as it arrives.

**Primary KPI.** 1 000 deduplicated 30-day-active users. Supporting: activation
rate, 30-day retention, source→active, community→active, referral conversion,
activated referral rate. Vanity metrics are ignored unless they predict active
users.

**Core question.** Is there a free, repeatable mechanism that continuously adds
real active Polish metalheads?

### What CrowdRelay must hold, and what is actually missing

The brief requires tracking: identity, consent, source, campaign, creative,
community, referrer, intent, activity, activation, last_activity, 30d_active —
deduplicated, with external platforms as execution surfaces and never as
durable truth.

Against the current schema:

| field | state |
|---|---|
| identity | `fans`, deduplicated on normalized email |
| consent | `fan_consents`, latest-wins per purpose |
| source | `fan_acquisition_events.source`, but derived from the campaign |
| campaign | `campaigns` + `smart_links.campaign_id` |
| referrer | `referral_attributions.referrer_fan_id` |
| intent | partial — `event_interests` only |
| activity | scattered across tickets, interests, Synesthesia, beacon sessions |
| **creative** | **missing** — no field distinguishes which post or image |
| **community** | **missing** — no field for *which* group, subreddit or Discord |
| **activation** | **missing** — the definition exists only in this brief |
| **last_activity** | **missing** — never materialized |
| **30d_active** | **missing** — the primary KPI cannot currently be computed |

Five real gaps, and the last three matter most: **the primary KPI of this
campaign cannot be measured by the system today.** Building the campaign before
they exist means running the operating loop blind, which is the one thing the
brief rules out.

Note also that `FanSignupInput` carries no `source`, `creative` or `community`
field at all — the channel is inferred from the campaign behind the smart link.
Community and creative therefore need either a campaign per community (cheap,
crude, works immediately) or explicit fields (correct, needs an API change).
Decide before the first post, not after.

---

## Resume checklist

1. `git status --short --branch`
2. Read this file's phase markers; the first phase not marked DONE is next.
3. `make check` before starting, so a pre-existing failure is not mistaken for
   a new one.
4. Work one sub-phase at a time; each ends green on `make check`, each phase
   ends green on `make ci`.
5. Update the phase markers in this file as part of the same commit as the code.
