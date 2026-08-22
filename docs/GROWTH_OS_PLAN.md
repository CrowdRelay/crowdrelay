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

## Phase 3 — growth debt detectors (NEXT)

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
- Incomplete distribution: a release plan with unfulfilled
  `viryaos_release_components`.
- Inactive channel: a `growth_metrics` series that has gone flat for longer
  than its cadence — already emitted as `StaleFeed` in Phase 1.
- Stale information: `viryaos_*_targets` rows whose verified contact data is
  older than the policy horizon.

Decide during Phase 3 whether this is one `growth_debt` context or predicates
added to existing contexts. Default to extending existing contexts — a new
context is only justified when it needs its own authority and quota.

---

## Phase 4 — one prioritized Next Best Action queue

- `GET /v1/admin/autopilot/next-best-actions` — a single ranked queue across
  every context, not a per-context list.
- Ranking inputs, in order: authority state (awaiting approval outranks
  observed), deadline proximity, `value_tier` of the affected metric, measured
  effect of the same action kind in the past (Phase 5), confidence, deviation
  magnitude.
- Hard cap the response. The point is the top handful, not thirty suggestions.
- Extend the `load_chief_of_staff` opportunity query's context allow-list to
  include `growth_metrics` — that alone surfaces metric-driven opportunities in
  the existing operator view for close to nothing.
- Each entry carries: reason, priority, expected impact (as measured deviation,
  never an invented currency amount), recommended action, authority, and what
  would happen if it is ignored.

---

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
