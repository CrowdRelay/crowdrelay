# The canonical learning loop

One document for the whole cycle, edge by edge. For each edge: what owns the
truth, what is derived from it, what `UNKNOWN` means there, and — the question
that matters most — **whether the value changes the next decision**.

A value that is computed, stored, and read by nothing is not part of the loop.
Several are marked *dormant* below. They are listed anyway, because a reader
who finds the plumbing and assumes the wiring is exactly how this system starts
lying to itself.

`scripts/test_brain_learning_loop_v1.py` checks that every symbol named here
still exists and that the dormant edges are still dormant. Wiring one up means
editing that gate in the same change.

---

## The loop

```
OBJECTIVE        North Star metric + declared objective  world_model.rs, goal.rs
  ↓
WORLD STATE      WorldModel, loaded once per cycle       operations/growth_intelligence.rs
  ↓
BELIEF           CausalModel: outcome model, τ(Y14),     brain/causal_model.rs
                 τ(Y30), Y14→Y30 bridge, calibration
  ↓
PREDICTION       predict_stats_with_treatment_for_target brain/causal_model.rs
  ↓
CANDIDATES       one per (template, target), scored by   application/…/growth_intelligence.rs
                 EFE for generation order only
  ↓
ECONOMIC VALUE   DecisionValue::from_stats               brain/decision_value.rs
  ↓
PORTFOLIO        submodular greedy on total(), WAIT      brain/portfolio.rs
                 competing as a candidate
  ↓
AUTHORITY        path-prefix auth, posture, approvals    api/lib.rs, autopilot/control.rs
  ↓
EXECUTION        action → outbox → executor → receipt    autopilot/runtime.rs
  ↓
REAL OUTCOME     provider delivery, fan provenance       measurement.rs
  ↓
MEASUREMENT      Y14 / Y30 windows, control arm swept    measurement/readiness.rs
                 in the same transaction
  ↓
CAUSAL UPDATE    apply_evidence_to_model over the        …/growth_intelligence/evidence_replay.rs
                 resolved_at delta, contrasted against
                 the experiment's control arm
  ↓
BELIEF UPDATE    checkpoint + delta replay               load_causal_model
  ↓
NEXT DECISION
```

---

## Edges, and what is actually true at each

### OBJECTIVE → WORLD STATE

- **Truth**: `growth_metric_points`, per platform and metric key.
- **Derived**: `WorldModel.north_star_current`, `north_star_this_month`,
  `GrowthTargetProgress`.
- **UNKNOWN**: a platform with no recent point is not zero. Feed health is
  reported separately (`/v1/admin/ops/connections`) because a connected feed
  that returns nothing looks identical to a quiet audience.
- **Influences the next decision**: yes. `GrowthStrategy::from_world_model_with_hysteresis`
  reads the trend, `from_world_model_with_posterior` may override that
  incumbent when a challenger's posterior clears P(Δ ≥ 1 fan) ≥ 0.6, and
  template priority within a strategy is ordered by measured platform yield.

### WORLD STATE → BELIEF → PREDICTION

- **Truth**: `growth_evidence` rows with `resolved_at IS NOT NULL` and
  execution status not `unknown`.
- **Derived**: the hierarchical Gamma-Poisson outcome model, the Y14 and Y30
  treatment-effect posteriors, the Y14→Y30 bridge, and the per-regime
  calibration trackers.
- **UNKNOWN**: an execution whose outcome cannot be established never enters.
  This is guarded twice on purpose — in SQL at the loader, and again in the
  causal layer via `CausalEstimand::includes_in_treatment_effect`.
- **CONFLICT**: an evidence row whose control arm is unresolved is held rather
  than replayed; the delta cursor moves past a row exactly once, so "wait" is
  the only recoverable answer.

### The randomised contrast

- Earned **per horizon**. `ControlMean` carries `Option<f64>` for Y14 and Y30
  separately, because they close sixteen days apart and an experiment spends
  most of its life resolved on one and pending on the other.
- A treated row whose horizon has no control mean is capped at
  `MatchedQuasiExperiment` and contrasted against nothing — honest weakness,
  not a randomised claim.
- The Y14→Y30 bridge is fitted only on pairs where both horizons agree about
  whether they are contrasted. A slope fitted between a control-adjusted Y30
  and a raw Y14 describes neither quantity.
- Earned **across batches**, not only within one. The learning cursor is
  `resolved_at` and the randomisation does not respect it: an experiment's
  treated units resolve when their own measurements finish, on different days,
  while the control arm resolves once alongside the first of them. Delta replay
  therefore fetches the control arm of every experiment in the batch by
  experiment id, ignoring the cursor — `load_control_arm_evidence`. Those rows
  are a **contrast only**; they were learned from when they first resolved, and
  the outcome model updates from every row in the learning batch, so replaying
  them would count them twice. `apply_evidence_to_model_with_contrast` keeps the
  two slices apart.

### PREDICTION → ECONOMIC VALUE

- `DecisionValue::total()` is exactly `pragmatic_value + risk_penalty +
  opportunity_cost + economic_value_fans + harm_fans`. Every term is in
  expected incremental Y30 fan-equivalents.
- `economic_value_fans` is the revenue term: a candidate's revenue prediction
  converted through `ValueExchange::minor_per_fan`, the tenant's learned
  revenue-per-fan rate. The exchange refuses to answer until it has ≥7 days
  and ≥20 new fans of history — until then the term is `None`, not zero.
- `harm_fans` is the harm term: the learned `harm:unsubscribes` +
  `harm:fan_suppressions` posterior means, subtracted. Complaints, refunds
  and cancellations are constraint inputs only — they have no honest
  fan-equivalent price and are not given one.
- `risk_penalty` is `None` — **not modelled**, not zero, and never derived from
  `uncertainty`.
- `uncertainty`, `evidence_quality`,
  `bridge_is_reliable` are **provenance** — until the uncertainty gate opens.
  Each resolved 30-day outcome is scored against the posterior its decision
  recorded (`calibration.y30_interval`); once 200+ are scored and 70–90% land
  inside the posterior's own 80% interval, exploit candidates that are not in
  an experiment carry `uncertainty_penalty = -0.674 × uncertainty` (valued at
  their posterior's 25th percentile). Shut, the term is `None`. Penalising
  uncertain candidates before the spread is verified is how a young learner
  stops learning; explore, learn and experimental candidates are never
  penalised. Every cycle's dispatch log says whether the gate is open and why.
  `evidence_quality` is stamped from the live experiment design for
  treatment-armed candidates — a dispatch under an active holdout records
  `randomized_holdout`, not the `observational` default.
- `contamination` was deleted rather than wired: at decision time the only
  readable value was a stale prior-round row — this cycle's assignment is
  persisted at dispatch — and `assignment_time_contamination` is stamped 0
  at creation. The live record is the assignment row's `final_contamination`,
  written at measurement and trace-joined one hop away.
- EFE never enters `total()`. EFE decides what is worth learning about;
  `total()` decides what is worth doing.

### ECONOMIC VALUE → PORTFOLIO

- Ranking is `total()` alone, and is independent of input order (including on
  ties). WAIT competes as a candidate rather than being a fallback.
- **WAIT competes with two of its four terms at zero.** `WaitCandidateValue`
  declares value-of-information, fatigue recovery, option value and opportunity
  cost. Only the first and the last are computed. The two that are not are both
  terms that would make waiting *more* valuable, so the brain is biased toward
  acting by however much a recovered audience is worth. Guessing a coefficient
  would be worse than the gap: an invented fan-equivalent number is the
  weighted soup `DecisionValue` refuses, and nothing downstream could tell it
  from a measured one.
- Tenant preference affects cadence, never economic value. A low-preference
  candidate with high `DecisionValue` stays selectable.

### EXECUTION

- **Truth**: `autopilot_actions.status` (lowercase vocabulary).
- **Projection**: `action_ledger.state` (uppercase), maintained by the
  `action_ledger_sync` trigger. One-way. `ActionState::from_action_status`
  and that trigger are pinned equal, and both are pinned to the column's CHECK,
  by `scripts/test_action_state_parity_v1.py`.
- **Independent**: `experiment_assignments.execution_status` (causal treatment
  realisation) and `community_posts.status` (provider delivery). These mirror
  the action in many cases and are *not* projections of it.
- **UNKNOWN**: "cannot establish whether the external side effect happened."
  Not a failure. Triggers reconciliation, never retry.
- **Premature vs provider-confirmed success**: `SUCCEEDED` is written at
  dispatch, before the provider confirms. `SuccessEvidence` is the derived fact
  that distinguishes the two, and it is a parameter of `legal_transition` so no
  caller can decide it by accident. A late failure corrects a premature
  success; against a provider-confirmed one it is `Conflict`.
- **CONFLICT**: recorded in the execution-report ledger as audit, surfaced to
  the operator, and commits **no** execution-derived success side effects.
- **Unreadable**: an action status this build cannot map is not a state. The
  receipt is audited and nothing moves — see `locked_action_state`.

### OUTCOME → MEASUREMENT

- A measurement is scheduled at provider-confirmed success, never at dispatch.
- Control units are never dispatched, so nothing schedules their measurement.
  `resolve_control_evidence` sweeps the control arm **in the same transaction**
  as the treated unit's measurement, so both carry the same `resolved_at` and
  land in the same delta batch.
- Evidence is marked resolved only when no measurement for the action is
  pending or processing, **and** the experiment's control arm has resolved (or
  its own 44-day window has elapsed).
- Randomised *design* becomes randomised *evidence* only when the outcome was
  read at the level the randomisation was performed at — see
  `measured_evidence_quality`.
- The same completion transaction merges `observed_metrics` onto the evidence
  row — one JSONB map of metric key to raw observed value, covering every
  learnable kind (`learnable_metric_key`), including the five `harm:*` keys a
  measurement collects across complaints, unsubscribes, suppressions, refunds
  and cancellations. A failed harm observation merges `None` — no keys —
  rather than zeros nobody earned.
- Replay folds each key into `CausalModel::metric_posteriors`, and those
  posteriors feed the next `DecisionValue`: `harm_fans` from the harm keys,
  `economic_value_fans` for revenue-denominated keys through the exchange.

### MEASUREMENT → CALIBRATION → PREDICTION

- Closed for the `OutcomeModel` regime: residuals are recorded per regime and
  `correct_prediction_by_regime(OutcomeModel, ..)` shifts the next outcome-model
  prediction.
- `Y14Bridged` and `Y30Direct` residuals are recorded and **not** applied as a
  correction; the treatment-effect posteriors are already fitted on the
  residual quantity. This is diagnostic, deliberately.
- Prediction error never becomes an economic reward term — pinned by
  `decision_value::tests::prediction_error_does_not_change_the_economic_value`.

---

## Provenance: what survives a decision

"What exactly did the brain know when it decided this?" is answerable, but only
partly, and not from one place.

**Persisted at dispatch.** `dispatch_predictions` holds the expected
fans, the expected Signal installs, the `expected_metrics` map, the
`DispatchContext` and the timestamps. `growth_evidence` holds the
evidence quality, the sample size, the strategy, the target key, the creative
family, and the `observed_metrics` map once measurements complete.

**Persisted on the decision.** Every selected candidate's `DecisionValue`
reaches `autopilot_decisions.input_snapshot` as the
`decision_value` provenance block (`portfolio::decision_provenance`) —
economic terms (`intrinsic_y30`, `economic_value_fans`, `harm_fans`, the
exchange rate used, `risk_penalty`, `opportunity_cost`, marginal
`adjustments`), epistemic terms (`estimation_regime`, `evidence_quality`,
`sample_size`, `uncertainty`, `uses_y30`, `bridge_confidence`,
`bridge_is_reliable`, and the candidate's own Y30 `posterior` — mean, std,
P(meaningful effect) — as it stood at decision time), policy identity, and the `competition` block naming
what else was considered and why each lost. The same value is serialised
into the `portfolio_pool` row. `input_snapshot.learning` records the
strategy prior vs applied pair (`strategy_source` = `prior` | `posterior` |
`default`).

Re-deriving the rest later does not recover it: the posteriors have moved,
so a re-derivation answers "what would the brain decide now", which is a
different question and looks identical in a report. The candidate's own
posterior is durable; what is still not is the causal model's full state
(every other key's posterior, the bridge, the harm model) — the
`belief_state` identity names which checkpoint it was, not its contents.

## Dormant edges

Written, never read on a decision path. Listed so nobody has to discover it.

| Value | Written by | Read by |
|---|---|---|
| `SelfAssessment` verdict | `/v1/control-plane/ops/attention` | operator only; changes no ranking, by design |

Wired or deleted since this table first listed them:

- `StateConditionedStrategyPosterior` — read by
  `GrowthStrategy::from_world_model_with_posterior`, gated on
  P(challenger exceeds the hysteresis incumbent by ≥1 fan) ≥ 0.6.
- `DecisionValue::contamination` — deleted; the truthful record is the
  assignment row's `final_contamination`.
- `WorldModel`: `best_performing_community`, `worst_performing_community`,
  `promoted_outreach_targets` — deleted; nothing read them, and the promoted
  count still reaches the engager as the snapshot's
  `unengaged_outreach_targets`.
- `GrowthEvidence::creative_family` — consumed by `update_family_effect` in
  evidence replay and the community-engager's Thompson sampling.

---

## Known weaknesses

1. **Uncertainty is gated, not yet exercised.** The penalty exists and opens
   only on 200+ outcomes scored against their decision-time posterior with
   honest 80% coverage. Until that record exists — and for decisions made
   before posteriors were recorded, it never will — ranking ignores spread.
2. **WAIT is under-valued by construction.** Two of its four declared terms are
   never computed, both in the direction that would favour waiting. The fix is
   to measure fatigue recovery, not to pick a number for it.
3. **Only the chosen candidate's posterior is durable per decision.** The
   `decision_value` block keeps its Y30 posterior (mean, std, P(meaningful))
   beside the regime, terms and competition, so a resolved outcome can be
   scored against what the brain believed. The rest of the causal model's
   state is named by checkpoint hash, not copied. See the provenance section.
4. **Goal-directed control is a ceiling and a posture, not a planner.** A
   declared objective that is `Behind` raises the dispatch ceiling by its pace
   gap (capped) and withholds the exploration boost; on track, nothing
   changes. The expected trajectory is the portfolio's own expected Y30,
   recorded per decision beside the goal — there is no separate trajectory
   model, deliberately. See `docs/GOAL_DIRECTED_CONTROL.md`.
5. **The loop is correct and barely exercised.** Almost no outcome has
   resolved. Most of the arithmetic above is right and untested by reality; no
   code change fixes that.
