# Goal-directed control: the contract for "100 durable fans in 21 days"

This is a contract, not a plan. Nothing here proposes a planner, and the one
thing this document is most concerned with preventing is a second brain.

## The finding this started from

CrowdRelay had **two definitions of the goal**, and they did not know about each
other. The table is kept as it was, because it is the reason for everything
below; the last row is what changed — see "What was built".

| | `domain::objectives::GrowthObjective` | `brain::world_model::GrowthTarget` |
| --- | --- | --- |
| Declared by | an operator | nobody — derived |
| Source | `growth_objectives` | a hardcoded fan-count table |
| Has a deadline | yes | no; a calendar month |
| Has a frozen baseline | yes | no |
| Scope | workspace / city / event / release plan | workspace |
| Refuses to guess | yes — `Unmeasurable { reason }` | no |
| Reaches the operator | yes: API, chief briefing | via `GrowthTargetProgress` |
| **Reaches a brain decision** | **no** (now: yes — the ceiling and the posture) | yes |

`GrowthTarget::from_fan_count` was the target the brain optimised toward, and
it remains the operator's monthly readout:

```rust
0..=99 => 20,    // new fans per month
100..=999 => 50,
_ => 100,
// north star target = max(north_star_current / 10, 5)
```

An operator could declare "100 durable fans by the 27th" through the objectives
surface, watch it turn `Behind`, and the brain would not have changed a single
decision — because nothing in `evaluate/` read an objective. The number the
brain was actually working toward was `max(current / 10, 5)`.

That was the gap. It was not that goal-direction was unbuilt; it was that the
built half and the deciding half were not connected.

## The loop the contract has to support

```
GOAL          declared target + deadline + scope
  ↓
BASELINE      the series value when the goal was declared, frozen
  ↓
REMAINING     target − observed, oriented by MetricDirection
  ↓
DEADLINE      time left, and the pace it implies
  ↓
ACTION SPACE  which templates can move THIS series at all
  ↓
PORTFOLIO     select against the remaining delta, not against a monthly bucket
  ↓
EXPECTED      the trajectory the selection implies
  ↓
ACTUAL        the series, measured
  ↓
REPLAN        the difference between the two, acted on
```

## What already exists

Most of it, and the parts that exist are the parts that are usually done badly.

- **GOAL.** `GrowthObjective { platform, metric_key, scope, direction,
  baseline_value, target_value, declared_at, deadline }`. Complete.
- **BASELINE.** Frozen at declaration, deliberately: "progress measured from a
  baseline that moves is not progress."
- **REMAINING and DEADLINE.** `assess_objective` returns `Met`, `OnTrack`,
  `Behind { shortfall, projected_value }`, `Missed`, or
  `Unmeasurable { reason }`. It refuses more readily than it guesses — no
  observation, or less than 72 hours elapsed, is `Unmeasurable`, not "on track".
  This is the hardest part of goal tracking and it is already right.
- **ACTION SPACE.** `GrowthStrategy::template_priority_for(world_model)` ranks
  templates by measured platform yield. `MetricPlatform` and
  `WorldModel::platform_growth` already say which platform a template moves.
- **PORTFOLIO.** `PortfolioOptimizer` selects on `DecisionValue::total()`, in
  expected incremental Y30 fans, with WAIT competing. Deadline-free, but the
  unit is already the unit a goal is denominated in.
- **ACTUAL.** `growth_metric_points` — the same series the objective is
  declared against. One source, no second reading.

## What was built

Three things, small on purpose, in the places the contract names.

1. **An objective the brain can see.** `WorldModel.objective` carries the live
   workspace-scoped objective with the nearest deadline, as `assess_objective`
   judged it (`crowdrelay_brain::goal::ActiveObjective`). The snapshot loader
   gets it from the same read the objectives endpoint serves
   (`infra/autopilot/objectives.rs::assessed_objectives`), so the operator and
   the brain see one verdict. City, event and release-plan objectives stay
   readouts: the portfolio selects for the whole workspace, and a city being
   behind is not a reason to send more everywhere. Met, missed and
   unmeasurable objectives steer nothing.

2. **A required pace.** `GoalPace::from_objective` is arithmetic on the
   assessment and the deadline: the remaining distance (the assessment's own
   `shortfall` when it states one), days left, the required pace per day, and
   the pace observed since declaration. The ratio of required to observed is
   dimensionless, so the series' units never have to be converted into Y30
   fans to be compared. No observation or no time left means no pace, not a
   guessed one.

3. **Two places the pace acts, and no others.**

   - **`PortfolioConfig::max_dispatches`, as a constraint.** On track, the
     sized budget stands. Behind, it is scaled by the pace ratio and clamped to
     `GrowthIntelligencePolicy::goal_max_dispatches` (default 10, twice the
     normal five). The ceiling is sized by the same metacognition and
     execution-health multiplier as the base, so a cautious or degraded brain
     stays so under a deadline, and a goal never shrinks the base. Every
     candidate still has to clear `min_marginal_value` on its own, and WAIT
     still competes: more slots, not lower standards.
   - **Exploration allocation.** Behind withholds the metacognition exploration
     boost on the EFE weights, so candidate generation leans on what is known
     to work. It changes which candidates rank first for generation; it does
     not change what any of them is worth.

   The pace must **not** enter `DecisionValue::total()`, and does not, nor the
   optimizer's marginal. Urgency is not value: a candidate is worth what it is
   expected to produce, and a deadline does not make a bad action better.
   Adding a deadline term to `total()` is the "weighted soup" the invariant in
   `decision_value.rs` exists to prevent, and it would make the brain dispatch
   harder exactly when it is losing, which is when its estimates are least
   trustworthy.

**Expected versus actual, and replanning.** The expected trajectory is the
portfolio's own: each decision's provenance carries a `goal` block with the
constraint (objective, state, pace, base and applied ceiling, whether
exploration was withheld) beside `planned_y30_this_cycle`, the selection's
expected Y30. The actual is the series. Replanning is the next cycle: the
objective is re-assessed from the series every cycle, so a gap that grows
raises the ratio and the ceiling with it, and a gap that closes returns the
budget to normal. Each cycle's `goal` line is also in the cycle report's
dispatch log.

What this does **not** do: sum expected Y30 over the "cycles remaining". The
autopilot polls every few minutes and most templates sit on cooldowns of hours
to days, so a cycle count is not a count of dispatch opportunities, and a
trajectory built on one would be a number with a decimal point and no
meaning.

## What must not be built

- **A second planner.** The portfolio optimiser is the selector. A goal supplies
  a constraint and a posture; it does not get its own ranking.
- **A second definition of progress.** `assess_objective` is the only one.
  `GrowthTargetProgress` may keep its monthly bucket as an operator readout,
  but if a declared objective exists it is the goal, and the derived table is
  not a competing answer to the same question.
- **A trajectory model.** "Expected trajectory" is the portfolio's own expected
  Y30, recorded per decision beside the pace it was chosen under. It needs no
  new estimator, and one would only be a second opinion about a number the
  brain already produces.
- **Deadline-driven safety overrides.** A deadline may raise a budget inside the
  envelope. It may never widen authority, skip an approval, or relax a
  circuit breaker. A goal the operator wants is not consent to an action they
  did not authorise.

## The honest state today

`scripts/test_goal_directed_control_v1.py` pins the wiring: the objective is
read in the brain by `goal.rs` alone, reaches the evaluation path only in the
portfolio stage and the exploration boost, and never appears in
`decision_value.rs` or the optimizer. Widening any of that has to change this
document and the gate in the same commit, and the section it has to change is
"What was built", not "What must not be built".

`GET /v1/control-plane/ops/goal` is the scoreboard for a run: the objective
and its pace, the expected new fans the brain's dispatches since declaration
were decided on beside how far the series actually moved (side by side, never
divided — different units), resolved and pending evidence against the 200
resolved rows the brain needs before uncertainty can enter selection, and how
long people take to approve what it drafts (standing grants and ladders
excluded), with the oldest item still waiting.

The wiring is exercised by unit tests, not yet by a real deadline. Whether the
raised ceiling produces more durable fans or only more dispatches is a
question for resolved outcomes, and the `goal` provenance block is there so
it can be answered.
