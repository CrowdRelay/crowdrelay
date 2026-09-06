# Goal-directed control: the contract for "100 durable fans in 21 days"

This is a contract, not a plan. Nothing here proposes a planner, and the one
thing this document is most concerned with preventing is a second brain.

## The finding this starts from

CrowdRelay has **two definitions of the goal**, and they do not know about each
other.

| | `domain::objectives::GrowthObjective` | `brain::world_model::GrowthTarget` |
| --- | --- | --- |
| Declared by | an operator | nobody — derived |
| Source | `viryaos_growth_objectives` | a hardcoded fan-count table |
| Has a deadline | yes | no; a calendar month |
| Has a frozen baseline | yes | no |
| Scope | workspace / city / event / release plan | workspace |
| Refuses to guess | yes — `Unmeasurable { reason }` | no |
| Reaches the operator | yes: API, chief briefing | via `GrowthTargetProgress` |
| **Reaches a brain decision** | **no** | yes |

`GrowthTarget::from_fan_count` is the target the brain optimises toward:

```rust
0..=99 => 20,    // new fans per month
100..=999 => 50,
_ => 100,
// north star target = max(north_star_current / 10, 5)
```

An operator can declare "100 durable fans by the 27th" through the objectives
surface, watch it turn `Behind`, and the brain will not have changed a single
decision — because nothing in `evaluate/` reads an objective. The number the
brain is actually working toward is `max(current / 10, 5)`.

That is the gap. It is not that goal-direction is unbuilt; it is that the
built half and the deciding half are not connected.

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
- **ACTUAL.** `viryaos_growth_metric_points` — the same series the objective is
  declared against. One source, no second reading.

## What is missing

Three things, and they are small on purpose.

1. **An objective the brain can see.** A port method that loads the *active*
   objectives for a workspace, and a `WorldModel` field carrying the assessed
   state of the one the brain is optimising for. The world model already
   carries `north_star`; it does not carry "and here is what someone asked for
   by when".

2. **A required pace, in the unit the portfolio ranks in.** From
   `shortfall` and time-to-deadline: expected incremental Y30 fans per cycle
   needed to arrive on time. This is arithmetic on values that already exist —
   `Behind { shortfall }` and `deadline − now` — not a model.

3. **One place the pace is allowed to act.** This is the decision that matters,
   and the wrong answer is the tempting one.

   The pace must **not** enter `DecisionValue::total()`. Urgency is not value:
   a candidate is worth what it is expected to produce, and a deadline does not
   make a bad action better. Adding a deadline term to `total()` is precisely
   the "weighted soup" the invariant in `decision_value.rs` exists to prevent,
   and it would make the brain dispatch harder exactly when it is losing —
   which is when its estimates are least trustworthy.

   The two legitimate places:

   - **`PortfolioConfig` as a constraint.** A pace requirement raises
     `max_dispatches` or the cost budget, within the existing safety envelope.
     Constraints are where non-fan-equivalent quantities already live.
   - **`DecisionMode` / exploration allocation.** Behind and short on time is a
     reason to exploit rather than explore. That changes which candidates are
     generated, not what any of them is worth.

## What must not be built

- **A second planner.** The portfolio optimiser is the selector. A goal supplies
  a constraint and a posture; it does not get its own ranking.
- **A second definition of progress.** `assess_objective` is the only one.
  `GrowthTargetProgress` may keep its monthly bucket as an operator readout,
  but if a declared objective exists it is the goal, and the derived table is
  not a competing answer to the same question.
- **A trajectory model.** "Expected trajectory" is the portfolio's own expected
  Y30 summed over the cycles remaining. It needs no new estimator, and one
  would only be a second opinion about a number the brain already produces.
- **Deadline-driven safety overrides.** A deadline may raise a budget inside the
  envelope. It may never widen authority, skip an approval, or relax a
  circuit breaker. A goal the operator wants is not consent to an action they
  did not authorise.

## The honest state today

`scripts/test_goal_directed_control_v1.py` pins the claim this document is
built on: no file under `crates/crowdrelay-application/src/autopilot/evaluate/`
reads an objective. When that stops being true, this document is the thing that
has to change in the same commit — and the section it has to change is "What is
missing", not "What must not be built".
