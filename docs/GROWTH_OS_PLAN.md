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

## Phase 1 — external metrics + trend/anomaly + `growth_metrics` context

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

### 1b — application (NEXT)

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

### 1c — infrastructure

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

### 1d — API and contract

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

### 1e — proof

- `make check`, then `make ci`.
- `scripts/` contract tests: add coverage asserting `growth_metrics` is
  provisioned disabled/observe and that the three context CHECK constraints
  agree with `AutopilotContext`.

---

## Phase 2 — two real provider slices

Model first, providers second. Pick the two with the best evidence-to-effort
ratio and **do not** add provider credentials to CrowdRelay for the others.

1. **Ticketing (first-party, zero new integration).** Derive series from
   `ticket_orders`/`ticket_order_items` per event: paid tickets, paid buyers.
   `value_tier: downstream`. This is the strongest possible metric and needs no
   external call — a worker step writes points on a schedule.
2. **Bandsintown.** `crates/crowdrelay-worker/src/event_sync/bandsintown.rs`
   already talks to the provider. Extend it to write trackers/interest points
   for the series attached to each synced event. Only fields the provider
   genuinely returns; if a field is absent, no point is written — never a zero.

Spotify / YouTube / social arrive through the Phase 1d ingest endpoint driven
by the existing n8n adapters. CrowdRelay does not grow OAuth flows for them
until there is a reason it must own the credential.

Reporting rule: a provider that cannot supply a number gets **no series**, not
a series full of zeroes.

---

## Phase 3 — growth debt detectors

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

## Resume checklist

1. `git -C /Users/wojciechbator/dev/crowdrelay status --short --branch`
2. Read this file's phase markers; the first phase not marked DONE is next.
3. `make check` before starting, so a pre-existing failure is not mistaken for
   a new one.
4. Work one sub-phase at a time; each ends green on `make check`, each phase
   ends green on `make ci`.
5. Update the phase markers in this file as part of the same commit as the code.
