# Brain incident: measured state and recovery gates — 2026-10-01

## Evidence boundary

This audit read GitHub main `4a980b90f2066aac0d5ef5d898e05564a0924477`,
Actions job logs, and public production endpoints around 12:43 UTC (14:43 Warsaw).
No production database, authenticated admin endpoint or worker log was accessed.
The two Inbox alerts were supplied by the operator. Other six alerts were not supplied.

Direct production GET `https://signal-api.virya.music/v1/meta` returned:
- API git SHA `67c597cc581a52c7fdac1b3cd44f141947be4c0d`.
- Schema version 388.
- Build timestamp `2026-09-30T23:27:54+02:00`.

The API version is confirmed; worker image identity still requires inspection.
Do not assume the two processes were deployed together.

## Live measurements

From `https://signal-api.virya.music/metrics`; database snapshot availability was 1.
These gauges are scoped to the API workspace in `ops/metrics_snapshot.rs`.

| Metric | Snapshot | Interpretation |
| --- | ---: | --- |
| Cycles / preceding 24h | 288 | Worker is scheduling work. |
| Cycles not successful / 24h | 172 | 59.7%; not a count of evaluation failures specifically. |
| Decisions / 24h | 413 | Decisions are not delivered actions or fans. |
| Actions created / 24h | 93 | Cohort of recently created actions. |
| Those actions currently failed | 45 | 48.4%; remaining states are not necessarily successes. |
| Awaiting approval | 2 | All ages. |
| Pending measurements | 1440 | Pending alone does not establish a backlog. |
| Oldest overdue pending measurement | 0 seconds | This snapshot does not show overdue pending work. |
| Successful measurements | 469 | All measurement kinds; not fan-learning observations. |
| Resolved growth evidence rows | 16 | Raw resolved-row count; not proof of an accepted posterior update. |
| Age of newest resolved evidence | 355031 seconds | About 98.6 hours at first read. |
| Age of newest community publication | 207012 seconds | About 57.5 hours; does not describe all social channels. |
| Agent outcomes processed / rejected in 24h | 32 / 11 | Rejection causes require inspection; not necessarily a faulty verifier. |
| Reddit halted | 1 | Read the halt reason; do not bypass the guard. |
| Communities joined / blocked on joining | 60 / 2 | Membership does not establish delivered distribution. |
| Reported activated_fans_30d | 11 | Current level, not new fans or incremental lift. |
| Signal installations / identified / push reachable fans | 3 / 1 / 2 | Distinct funnel quantities. |

The supplied Inbox shows 22,923 decisions and `causal_observations=0`.
The 22,923 value is cumulative, unlike the 413 decisions in the last day.

## What explains the incident

1. PR [#408](https://github.com/CrowdRelay/crowdrelay/pull/408) records a production
   worker error since September 30 22:32 UTC:
   `invalid UNION/INTERSECT/EXCEPT ORDER BY clause` (SQLSTATE 0A000).
   The growth-debt query fix merged in [#405](https://github.com/CrowdRelay/crowdrelay/pull/405).
   That production diagnosis was recorded by the PR author; this audit did not
   independently reread the worker log. The live API still runs the older release.
2. Current main CI [36862239971](https://github.com/CrowdRelay/crowdrelay/actions/runs/36862239971)
   failed in repository checks. Job 110369672945 contains a rustfmt diff at
   `github_registry_sync.rs:225`. Apply the exact formatting; no policy gate
   needs weakening. Subsequent gates were not proven by that failed run.
3. Publish run [36862748355](https://github.com/CrowdRelay/crowdrelay/actions/runs/36862748355)
   reports success, but its build, anchor and manifest jobs were skipped.
   Green workflow status alone is not an image or deployment receipt.
4. #408 also records an unset `CROWDRELAY_WATCH_PAGE_ORIGIN`, 69 capture-eligible
   clicks going directly to YouTube in the preceding week, and no fan signup
   since September 14. These counts/configuration are earlier reported findings,
   not a fresh database read here. A YouTube visit can still have audience value;
   bypassing capture loses the owned-fan conversion opportunity.
5. The code's `causal_observations` reads `brain_state.state.fans.global.n`.
   It counts fan-outcome observations, not all learning and not causal
   identification. Separate platform metric and treatment-effect learners exist.
   A mature attributed measurement of **zero** fans updates this posterior.
   Therefore “no signup” cannot by itself explain zero observations.
6. Nonempty evidence can be ignored by outcome-basis, context, execution,
   maturity or cursor gates. The previous “brain learned: posterior updated”
   log fired for any nonempty delta, even a replay with zero updates. This patch
   labels that event as a replay and points to the existing per-learner counters.

The exact partition of the 16 resolved rows remains unmeasured. Plausible branches
include historical `workspace_window` evidence, non-fan metrics, missing fan
outcomes, invalid contexts, replay/persistence failures or a stale checkpoint.
Do not select one without row-level evidence.

## Recovery sequence and acceptance gates

| Order | Work | Required proof |
| --- | --- | --- |
| 1 | Restore CI and ship the known query/learning fixes through normal release gates. | Successful exact-commit CI, actual API/worker image digests, deployed identities, migrations applied. |
| 2 | Verify evaluation. | Inspect `/v1/admin/ops/cycles?state=degraded` and the phase-specific worker warning; 12 consecutive completed cycles without evaluation failure (about one hour at current cadence). Alert closure alone is weaker. |
| 3 | Reconcile measurement and learning. | Partition resolved/pending/abandoned measurements and accepted/skipped evidence; trace at least one valid mature outcome, including zero, into persisted `fans.global.n`. |
| 4 | Restore eligible distribution. | Explain the Reddit halt and failed-action reasons. Use a permitted channel with a delivery receipt; preserve approval, consent, moderator and provider limits. |
| 5 | Verify capture. | Confirm deployed website capture support, set the tenant's watch origin in both API and worker configuration, and verify eligible owned-video redirect, visitor continuity and consented signup attribution. Do not count test identities as growth. |
| 6 | Prove feedback changes decisions. | Record action ID, actual publication, tracked link, observed result/window, accepted update counts, stored checkpoint identity, and the next decision's changed value/rank or allocation. Repeat/restart must not relearn the same observation. |
| 7 | Prove audience value. | Repeated delivered campaigns yield fresh attributable fans and meaningful return activity; platform outcomes remain separate from owned signups and from claims of incremental effect. |

Publication starts the fan observation clock. Current code waits 3 or 14 days
for acquisition, and 14+30 days for the mature survival cohort. First exploit
already mature valid observations. Do not shorten those windows to clear the alarm.
An active account surviving 30 days is not proof of engagement.

## Read-only database triage

Run with an existing authorized read-only connection, setting `workspace_id` to
the intended tenant UUID. These statements were checked against source column
names, not executed against production in this audit. No personal data or payloads
are selected. Keep the transaction read-only and bounded; a timeout is a finding.

```sql
-- psql -v ON_ERROR_STOP=1 -v workspace_id=<tenant-uuid> ...
BEGIN READ ONLY;
SET LOCAL statement_timeout = '10s';
SET LOCAL lock_timeout = '1s';

SELECT module, updated_at,
       state #>> '{fans,global,n}' AS fan_outcome_observations,
       state -> 'evidence_cursor' AS evidence_cursor
FROM brain_state
WHERE workspace_id = :'workspace_id'::uuid
  AND module = 'causal_model';

SELECT measurement_kind, status, last_error_kind, count(*) AS measurements,
       min(due_at) AS earliest_due_at, max(finished_at) AS newest_finished_at
FROM autopilot_measurements
WHERE workspace_id = :'workspace_id'::uuid
GROUP BY measurement_kind, status, last_error_kind
ORDER BY measurement_kind, status, last_error_kind;

-- Match the evidence reader's fallback; stored observed_fans alone is incomplete.
SELECT ge.outcome_basis,
       ge.resolved_at IS NOT NULL AS resolved,
       count(*) AS evidence_rows,
       count(*) FILTER (
           WHERE COALESCE(ge.observed_fans, dp.observed_new_fans) IS NOT NULL
       ) AS with_fan_observation,
       count(*) FILTER (
           WHERE COALESCE(ge.observed_fans, dp.observed_new_fans) = 0
       ) AS with_measured_zero,
       max(ge.resolved_at) AS newest_resolved_at
FROM growth_evidence ge
LEFT JOIN dispatch_predictions dp
  ON dp.workspace_id = ge.workspace_id AND dp.action_id = ge.action_id
WHERE ge.workspace_id = :'workspace_id'::uuid
GROUP BY ge.outcome_basis, ge.resolved_at IS NOT NULL
ORDER BY ge.outcome_basis, resolved;

SELECT status, last_error_kind, count(*) AS recent_actions
FROM autopilot_actions
WHERE workspace_id = :'workspace_id'::uuid
  AND created_at > now() - interval '24 hours'
GROUP BY status, last_error_kind
ORDER BY recent_actions DESC;
COMMIT;
```

Then inspect selected action IDs with the actual lineage predicates in
`measurement/observation/attributed_fans.rs`: direct action, shared trace and
agent-task outcome lineage. A join only on the measured parent misses child posts.
Use the worker's `evidence replay: posterior update summary` counters separately:
`outcome_updates`, `y14_treatment_updates`, `y30_treatment_updates`,
`bridge_updates`, `metric_updates`, `family_updates`.
Positive counters establish in-memory updates; also verify the saved checkpoint
and the next cycle after restart. A content hash can change just because a cursor
advanced, so a changed hash alone does not establish learning.

Do not relabel historical workspace-wide results as action attribution, reset
the posterior, or blindly retry terminal actions. Any repair/replay must preserve
evidence identity and avoid duplicate credit.

## Product direction after recovery

The operator needs one explainable source → delivery → visitor → conversion →
activation → meaningful return → referral funnel. Every transition needs its
own receipt and every missing transition a reason/owner. Keep decision count,
draft count and model-call count as costs, never growth achievements.

Optimize decisions toward retained, engaged fans while exposing source-specific
cost, uncertainty and valid comparisons. Explore within existing budgets; retire
or deprioritize ineffective lanes from measured outcomes, including zeros.
Platform follows/listens are useful distinct outcomes, but their totals are not
deduplicated people and observational attribution is not proven incrementality.

A first update clears “never learned”; it does not earn a 9/10 product score.
The gate is repeated delivery and fan value, followed by demonstrably better
subsequent choices. No production repair or audience uplift is claimed by this PR.
