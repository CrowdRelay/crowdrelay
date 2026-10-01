# Platform measurement feedback checkpoint — 2026-10-01

Audience-growth rating remains **4/10**; engine rating remains approximately
**7/10**. Neither code changes nor a green test suite establish real fan growth.
The earlier numerical fan examples were illustrative, not live measurements.

## Confirmed defect and resulting behavior

`release_channel_lift_14d` previously added all release-series contrasts into
one number, then taught the `release_channel_lift` posterior. YouTube views
could compensate for lost Spotify followers. Different units were being
treated as one outcome. The second pre-period reading also had no lower time
bound, so an old feed could supply both baselines and manufacture a lift.

Each complete series now retains its series ID, platform, metric, direction
and original-unit contrast in the outcome transaction. Evidence and forecasts
use separate `release_channel_lift:<platform>:<metric>` keys. A mixed result
cannot classify the action as improved: the aggregate verdict is neutral, or
worsened when attributable actionable harm exists. The existing scalar is
retained solely as a compatibility readout. A single lower-is-better series
orients the verdict while preserving the signed raw fact.

Historical mixed-unit evidence/checkpoints remain inspectable, but that key
no longer learns or appears in forecasts. A release-series contrast carries
observational evidence quality even if the action also belongs to a randomized
fan experiment: assignment of that experiment does not identify the release
series' counterfactual. A missing/stale baseline is unavailable, never zero.
Future points cannot become observations. Arithmetic casts precede BIGINT
subtraction.

## Verification and practical limits

Regression proofs cover mixed-unit classification, direction, invalid vectors,
legacy forecast exclusion, evidence-quality caps, and cursor-idempotent replay.
Native PostgreSQL tests follow observation → atomic completion → evidence →
distinct forecasts → checkpoint → restart, including duplicate completion.
The prior checkpoint's final native preflight fixture used a credentials table
shape incompatible with another test; it now creates the foreign table in an
owned schema and drops that schema afterward. Production preflight is unchanged.

This repairs the identity of feedback. It does **not** establish that a release
caused the platform change, retrofit historical mixed sums into identifiable
series, or prove an objective-specific allocator uses these forecasts to win
new followers. No arbitrary exchange rate converts views or followers to
engaged fans. Existing raw evidence storage is not a claim of bounded total
storage or of complete-cycle capacity at 50,000 contacts / 100,000 cycles.

## Remaining gates toward 9/10

1. Trace a live eligible action through delivery, attributed audience arrival
   on the intended platform, a mature observation, learning and the next
   eligible decision. Prove where actual traffic stops.
2. Add objective-specific allocation with platform/metric identity and honest
   uncertainty. Separate audience acquisition from consumption metrics; retain
   fan safety constraints and explain why each eligible action was selected.
3. Establish metric-matched counterfactuals before calling platform lift causal.
   Close late-control feedback without replaying an already-consumed outcome.
4. Measure full-cycle latency, contention and working-set growth with realistic
   contact/evidence volumes. Bounded SQL reads alone do not prove this SLO.
5. Confirm repeated, sustained audience gains and returns using actual platform
   observations. Until then, keep the product rating unchanged.
