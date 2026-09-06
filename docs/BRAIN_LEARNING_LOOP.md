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
OBJECTIVE        North Star metric + monthly target      world_model.rs
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

- **Truth**: `viryaos_growth_metric_points`, per platform and metric key.
- **Derived**: `WorldModel.north_star_current`, `north_star_this_month`,
  `GrowthTargetProgress`.
- **UNKNOWN**: a platform with no recent point is not zero. Feed health is
  reported separately (`/v1/admin/ops/connections`) because a connected feed
  that returns nothing looks identical to a quiet audience.
- **Influences the next decision**: yes. `GrowthStrategy::from_world_model_with_hysteresis`
  reads the trend, and template priority within a strategy is ordered by
  measured platform yield.

### WORLD STATE → BELIEF → PREDICTION

- **Truth**: `viryaos_growth_evidence` rows with `resolved_at IS NOT NULL` and
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
  opportunity_cost`. Every term is in expected incremental Y30 fan-equivalents.
- `risk_penalty` is `None` — **not modelled**, not zero, and never derived from
  `uncertainty`.
- `uncertainty`, `contamination`, `calibration_bias`, `evidence_quality`,
  `bridge_is_reliable` are **provenance**. None enters `total()`. That is a
  real gap, taken deliberately: penalising uncertain candidates in a system
  that has resolved almost no outcomes is how a young learner stops learning.
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

- **Truth**: `viryaos_autopilot_actions.status` (lowercase vocabulary).
- **Projection**: `action_ledger.state` (uppercase), maintained by the
  `viryaos_action_ledger_sync` trigger. One-way. `ActionState::from_action_status`
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

**Persisted at dispatch.** `viryaos_dispatch_predictions` holds the expected
fans, the expected Signal installs, the `DispatchContext` and the timestamps.
`viryaos_growth_evidence` holds the evidence quality, the sample size, the
contamination estimate, the strategy, the target key and the creative family.
Between them, most of the provenance a reader needs is durable.

**Not persisted at all.** `DecisionValue` is computed per cycle, ranked on, and
dropped. So `estimation_regime`, `bridge_confidence`, `bridge_is_reliable`,
`decision_mode`, and the `total()` the portfolio actually sorted by exist only
for the length of the cycle that produced them.

Re-deriving them later does not recover them: the posteriors have moved, so a
re-derivation answers "what would the brain decide now", which is a different
question and looks identical in a report.

`estimation_regime` is the one that matters most, and it is one column. Without
it you cannot tell whether a prediction of 3.2 fans came from the outcome
model, from a Y14 bridge — which the optimizer docks 20% when the bridge is
uncalibrated — or from Y30 directly. Those are three different claims and the
brain treats them differently. The vocabulary already exists and is stable:
`EstimationRegime::as_str` / `parse`, and `y30_direct` / `y14_bridged` /
`outcome_model` are already written to the calibration trackers.

Deliberately not added here. A column is a migration and a write path, and the
question of whether the rest of `DecisionValue` should travel with it is a
design decision rather than a correctness fix.

## Dormant edges

Written, never read on a decision path. Listed so nobody has to discover it.

| Value | Written by | Read by |
|---|---|---|
| `StateConditionedStrategyPosterior` | `apply_evidence_to_stored_strategy_posterior` (sole writer) | nothing — threaded through candidate generation and discarded |
| `DecisionValue::calibration_bias` | never populated | never read; `load_calibration_bias` exists on the port and has no caller |
| `DecisionValue::contamination` | `with_contamination`, brain tests only | never read |
| `GrowthEvidence::creative_family` | dispatch | nothing decides on it |
| `WorldModel`: `discovered_communities`, `active_communities`, `avg_community_engagement_bps`, `best_performing_community`, `worst_performing_community`, `pending_outreach_targets`, `promoted_outreach_targets`, `engaged_outreach_targets` | the snapshot loader, two dedicated queries per cycle | nothing — and `WorldModel` is neither persisted nor served, so unread here is unread everywhere |
| `SelfAssessment` verdict | `/v1/control-plane/ops/attention` | operator only; changes no ranking, by design |

---

## Known weaknesses

1. **No economic use of uncertainty.** Recorded above as deliberate. It stops
   being defensible once there is enough resolved evidence to distinguish a
   tight estimate from a wide one.
2. **Dormant strategy learning.** The posterior accumulates history nothing
   consumes. Either wire it into exploration allocation or delete it; leaving
   it indefinitely is the state that invites someone to assume it works.
3. **WAIT is under-valued by construction.** Two of its four declared terms are
   never computed, both in the direction that would favour waiting. The fix is
   to measure fatigue recovery, not to pick a number for it.
4. **A decision's own reasoning is not durable.** `DecisionValue` never
   reaches storage, so `estimation_regime`, the bridge confidence and the
   `total()` the portfolio sorted by survive only the cycle. Re-deriving them
   answers "what would the brain decide now" and looks the same in a report.
   See the provenance section.
5. **No goal-directed trajectory.** "100 durable fans in 21 days" has a
   baseline, a remaining delta, a feasible action space and a portfolio
   strategy. It has no expected trajectory and no replanning, so the deadline
   cannot change what the brain does. Deliberately not built — a second planner
   is worse than none.
6. **The loop is correct and barely exercised.** Almost no outcome has
   resolved. Most of the arithmetic above is right and untested by reality; no
   code change fixes that.
