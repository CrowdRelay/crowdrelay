# Growth Operating System — implementation plan

Multi-session plan. CrowdRelay already executes planned actions well; this work
adds the layer that decides *what is worth doing now*. It is deliberately split
into vertical slices that each ship value on their own, because the target end
state (observe → detect → recommend → execute → measure → learn) is not
something to land in one pass.

Read this file first when resuming. Every phase lists the exact files it
touches and the gate that proves it.

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

## Phase 5 — measurement and honest attribution

- Extend `viryaos_autopilot_measurements.measurement_kind` with growth-metric
  outcome kinds (e.g. `growth_metric_velocity_7d`), following migration 0049's
  pattern for the CHECK constraint.
- After an action for a series settles, compare the series' subsequent velocity
  against the pre-action baseline and record `improved`/`neutral`/`worsened` in
  `viryaos_autopilot_outcomes` — the columns already exist.
- The chain `action → campaign/channel → click → signup → ticket → merch` is
  only assembled where a real join key exists: `click_events`,
  `referral_attributions`, `merch_order_facts`, `fan_acquisition_events`,
  `viryaos_beacon_native_session_attribution`.
- Where a link is missing, the read model returns an explicit
  `evidence: insufficient` marker with the reason. **Never** interpolate a
  path, never present correlation as attribution, never fill a gap with a
  plausible number.

---

## Phase 6 — operator brief

- Extend `load_chief_of_staff` rather than building a dashboard: what changed
  (24h/7d), what is at risk (deadlines, declining downstream metrics, stale
  feeds), top opportunities, what ran automatically, what needs approval.
- Delivery via the existing outbox → n8n path. No new scheduler.
- The brief is a readout of already-actionable data, not a reason to store a
  new denormalized copy of it.

---

## Phase 7 — learning

- Feed measured outcomes back into ranking: an action kind that has repeatedly
  measured `worsened` for a context loses priority; one that measures
  `improved` gains it.
- Keep the adjustment bounded, explainable and stored as data, not as a model.
  An operator must be able to read why a suggestion was ranked where it was.
- Authority never widens automatically. A context is promoted from `observe` to
  `recommend` to `require_approval` to `bounded_auto` by a human, informed by
  the measured record.

---

## Phase 8 — audit

Only after Phases 1–7 are done. Five separate passes, each written up in this
file with findings and their resolution; the work is not finished until all
five are clean.

1. **Correctness** — does each rule do what it claims on real data, including
   the boundary cases (absent history, backfills, out-of-order delivery, clock
   skew, workspace isolation, replay)?
2. **Usefulness** — would an operator actually act on what the queue surfaces,
   or is it noise? Measure against real VIRYA data, not fixtures.
3. **Feature completeness** — every numbered requirement of the original brief
   accounted for: metrics, trends/anomalies, opportunities, next best action,
   measurement/attribution, growth debt, operator brief.
4. **Performance** — snapshot and read-model queries under realistic row
   counts; index coverage; no per-series N+1; bounded response sizes.
5. **Code quality** — layering rules held, ratchets respected, no dead
   abstraction, comments explain the non-obvious rather than restating code.

## Phase 9 — control-plane management and monitoring

Last. In `crowdrelay-control-plane` (the operator/infra plane, never
tenant-critical), add a thin management and monitoring layer over the Growth
OS: series health (which feeds are live, stale, or never reported), detector
throughput (decisions and actions per context per day), authority state per
context, and the measured outcome record. Read-only over CrowdRelay's API
contract; the control plane must not become a second source of truth, and no
business policy moves into it.

## Resume checklist

1. `git -C /Users/wojciechbator/dev/crowdrelay status --short --branch`
2. Read this file's phase markers; the first phase not marked DONE is next.
3. `make check` before starting, so a pre-existing failure is not mistaken for
   a new one.
4. Work one sub-phase at a time; each ends green on `make check`, each phase
   ends green on `make ci`.
5. Update the phase markers in this file as part of the same commit as the code.
