# Growth review — 2026-10-01

## Verdict and scoring boundary

Baseline: backend main `da55a3d0` (#403), including #404's durable learning
checkpoint. This is a code audit, not fresh production telemetry.

**Audience-growth product: 4/10. Operational engine: about 7/10.**
These are judgment scores, not measured KPIs. Neither is increased by this PR.
The goal is 9/10 in both categories; working machinery earns that only when its
actions repeatedly produce new audiences and retained relationships, and the
evidence demonstrably improves the next decision without reducing throughput.

The system has attribution, consented fan capture, platform series, learned
allocation and delivery machinery. It still has no reviewed live proof of a
repeatable action → delivered exposure → new audience → meaningful return loop.
Tasks, drafts, contacts, API attempts and cycle counts do not establish growth.
Spotify and other social audiences are valid outcomes alongside Signal. Report
them separately; platform accounts are not deduplicated people, and a correlated
increase is not per-action causal attribution.

## Demonstrated breaks repaired in this checkpoint

| Break | Product consequence | Repair and evidence |
| --- | --- | --- |
| The Y14→Y30 bridge required both horizon timestamps to be new in the same batch. | Normal experiments settle separately. Their durability pair could never teach the bridge, leaving early-growth ranking on an uncalibrated prior. | Learn when the last member of the same action's pair arrives, in either order. Keep matching contrast definitions and per-horizon treatment gates. Tests cover both orders, full/delta agreement, repeat/metric-only deltas, checkpoint restart and the next decision value. |
| Fan observation windows began at publication, but claims could happen before that window finished. | A late-published draft could settle a partial acquisition count or the mature subset of a still-immature retention cohort as a final result. Three ordinary retries cannot cover a fortnight/month of maturation. | In one claim transaction, inspect only the locked batch (at most 100). Move immature measurements to actual readiness: publication/action completion + 3/14 days, or 14-day acquisition + 30-day survival. Preserve attempts and allow ready siblings to proceed. Reuse the observer's exact trace/task lineage SQL. |
| A malformed stored dispatch context taught the default context. | One bad row could teach a real strategy/audience cell a result observed under an unknown situation. | Exclude that row from learning, preserve it for inspection, and consume its read watermark. Valid siblings still teach. An all-malformed batch does not fall back to legacy evidence or repeat the read indefinitely. |
| A fan conversion inside a shifted window could be timestamped after the observation. | Future arrivals could look like genuine acquisition; account age could stand in for age of the attributed conversion. | Bound conversion/account timestamps by observation time. Count thirty-day survival from the attributed conversion. |

No historical posterior is discarded to deploy these changes. A pair that was
already consumed under the old bridge gate is not silently relearned: repairing
historical missed pairs requires a separately reviewed, bounded bridge rebuild.
Malformed-row repair must stamp a new observation watermark or use a reviewed
replay; editing old JSON alone does not make consumed history new evidence.

## What remains short of 9/10

| Area | Current gap | Required proof |
| --- | --- | --- |
| Distribution | A successful artifact/task is not proof that an eligible lane actually delivered to an audience. | Campaign-level denominator: requested, eligible, approved, dispatched, delivered, destination-clicked, joined/followed, returned. Explain every hold, unconfigured lane, retry and exhausted attempt. Provider receipt is the delivery fact. |
| Acquisition | Low-friction owned capture now exists; its live conversion yield remains unproven. External platform gains can be visible without action-level attribution. | Several repeatable delivered campaigns produce fresh audience gains. Show unique tracked visitors, attributed signups, per-platform deltas and uncertainty; use valid comparisons for incremental effects. |
| Retention | `durable_fan_growth_30d` currently measures account survival, not meaningful engagement. Source/content retention readers also use activity, so these are different quantities. | Keep those labels distinct. Establish immutable meaningful-activity receipts and a comparable mature cohort, with current consent/revocation respected. Do not call an untouched open account an engaged fan. |
| Learning | New pairs can reach the ranking value; arbitrary late commits and later-arriving control contrasts still need a receipt/reconciliation protocol. | Trace real completed experiments through accepted evidence, persisted posterior identity and the next actual selection. Transactional receipts should support idempotent recovery and late observations without history scans. |
| Capacity | Indexed deltas and compact state fix demonstrated costs, but do not prove full-cycle capacity. | Native mixed-tenant small/large comparisons: complete cycle p95/p99, queue age, lock waits, memory and delivered outreach per hour under the same provider limits. User-supplied example sizes are not requirements. |

## Next checkpoint selection

Local validation: 17 tests of the actual pure evidence-replay implementation
against the complete brain/domain crates passed with serialization intact.
The two new lifecycle/ranking regressions fail against the baseline replay code.
The exact claim and attributed-count SQL passed both trace/task query shapes in
PostgreSQL/PGlite fixtures containing 100,000 additional future measurements per
shape, including readiness, attempts, ready siblings, workspace isolation and
future/conversion-age guards. This is query behavior, not a full-cycle latency
or concurrency proof. Six new migrated native PostgreSQL regressions are selected
explicitly in CI; full workspace verification is performed there.

Follow a real, eligible promotion from source through its delivery lanes and
tracked destinations. Locate the first missing receipt/transition rather than
add another planner. Close delivery → clicks → platform outcome/owned conversion
and make lane blockers visible to the operator, without competing with the
experienced booker or bypassing consent, moderator holds and provider limits.

Maintain larger reviewable checkpoints. A code fix earns completion of a named
acceptance gate; it does not automatically earn a higher product score.
