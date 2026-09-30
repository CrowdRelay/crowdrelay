# Growth review — 2026-09-30

## Verdict

CrowdRelay has substantial execution, attribution and learning machinery. It is
not yet a proven audience-growth product. A successful action, a generated post,
an imported contact or another brain cycle is not a new listener or fan. The
critical missing proof is a repeatable live path from system action to delivered
exposure, audience gain and a subsequent useful interaction.

Success includes Spotify followers and audiences on other socials. The user's
185→300 Spotify example is illustrative, not an observed baseline or target.
A verified gain is meaningful even if Signal stays flat.
Report each platform separately: aggregate platform accounts are not deduplicated
people, and a follower increase alone does not prove the system caused it.

## Evidence boundary

Code review baseline: backend main `520127e4f15638797d7f9f465987efa3fc1708dc`
(#400). Its CI completed successfully. The production numbers below come from
the earlier read-only audit recorded in this directory at approximately 07:25 UTC,
running revision `55e9a306`; they are not fresh production observations of #400.
That audit recorded 2,813 staged contacts, 20 active fan accounts, 14 signups and
11 activated fans in 30 days, 18 reachable consented fans, two active push
endpoints, four posted community rows in seven days and 13 held rows after
moderator removals. It did not establish incremental Spotify/social growth.

## Highest-priority gaps

| Priority | Gap and evidence | Next acceptance condition |
| --- | --- | --- |
| P0 | Live distribution has not been proven end to end. Community holds and very small owned reach can leave a busy system with little actual distribution. | One eligible campaign records provider delivery/post receipts, destination clicks and fresh platform outcomes. Show exactly which lanes are held, unconfigured, failed, exhausted or delivering. Keep moderator and consent holds effective. |
| P0 | Portfolio success and allocation need to match the audience goal. The loader supports per-platform growth and portfolio-wide north stars, but allocation still assesses the configured single north-star series. Initializing/regressing states scale dispatch to 0.3/0.5. Actual deployed configuration and decisions were not inspected here. | Replay a Spotify 185→300 scenario with flat Signal. Verify the platform gain reaches the scorecard and effective decisions; explain every budget reduction. Do not remove relationship/safety constraints to achieve throughput. |
| P0 | Provider data can block planning. The shared snapshot loader eagerly loads social history with error propagation; direct bigint casts meant one malformed reach could fail the entire read. | Invalid optional metrics remain unknown; social history and scorecard continue returning genuine outcomes. This checkpoint implements that behavior and adds regression coverage. |
| P0 | Growing history is not yet covered by an end-to-end capacity guarantee. One monthly baseline materialized comparable cycle history before choosing two rows. Other queries, provider waits, candidate generation, locks and queues still require measurement. | Fix the demonstrated read and then run a native PostgreSQL workload with 50,000 contacts and 100,000 cycles, mixed tenants and all enabled contexts. Record p50/p95/p99 cycle/read latency, queue age, delivered outreach per hour, query plans, lock waits and memory; compare with a small baseline under the same provider limits. |
| P1 | Learning needs stable evidence. Historical post activation was based on the latest action; a later return outside the first 30 days could erase an earlier activation. Acquisition-only ordering also favored signup volume over activation. | A historical cohort retains its activation evidence after a later return. Activated examples rank first. Current consent/account state still governs eligibility. Implemented in this checkpoint. |
| P1 | Metacognition does not accumulate its streak. The loader constructs a fresh monitor each snapshot. `learning_cycles` therefore stays at 0/1; `exploration_boost()` includes a progressive escape bonus based on that counter, despite the loader comment saying consumers depend only on assessed state. | Persist state after completed cycles, never during preview. Verify preview is read-only, retry is idempotent, workspace state is isolated and the streak changes exploration as intended without duplicate evidence learning. Fixed in the final continuity checkpoint below. |
| P1 | Durable causal learning exists but live improvement is unproven. Checkpoint/delta replay, per-horizon cursors and randomized contrasts have dedicated code and regression tests. They do not prove that production experiments resolve or influence the next dispatch. | Trace one resolved experiment through its evidence cursor, model checkpoint hash, changed posterior and next selected action. Verify one evidence horizon is learned once; randomized control remains available across batches; missing/corrupt checkpoint recovery has a bounded cost. |

## Earlier checkpoint (#401) and measured scope

The monthly activation baseline now uses two ordered `LIMIT 1` probes with a
covering partial index on workspace, metric and cycle time. On a synthetic
100,000-row PGlite PostgreSQL 18.3 fixture, the original plan scanned/materialized
90,000 comparable readings. The new plan used index-only scans returning zero
and one row. Recorded execution times were approximately 150 ms and 0.27 ms.
These WASM fixture timings are diagnostic evidence, not production latency claims.
`verify-history-scaling.sql` reproduces the 100,000-cycle comparison in a
transaction-local temporary table on native PostgreSQL and rolls back afterwards.
The index takes storage and write maintenance; ordinary migration index creation
also blocks writes to this table during its build. Measure native build duration
and schedule the migration accordingly before deployment.

Five focused Rust domain tests passed with serialization annotations removed for
the standalone harness. Exact social-history and hook SQL passed PostgreSQL
fixtures covering malformed metadata, historical activation, activation-first
ranking, other tenants, future sources/conversions/consent and current revocation.
The existing canonical helper from migration 0383 was used. A long-lived session
still preserves only its latest timestamp: historical activation remains a lower
bound if that was the only earlier action. An immutable activity receipt would
close that separate data-retention gap.

Committed Rust tests extend the migrated hook-scorecard fixture and cover monthly
baseline semantics in a transaction-local temporary table. Full Cargo/Clippy and
the migrated PostgreSQL suite were not run in the partial local snapshot; CI must
run them. No production deployment or fresh audience increase is asserted.

## Next substantial checkpoints

1. Prove distribution and platform success: expose the effective growth goal and
   lane blockers; validate Spotify/social gains alongside Signal; measure real
   delivered campaigns and clicks. Separate platform correlation from attributed
   and randomized incremental outcomes.
2. Make learning continuity durable: fix the metacognition streak and demonstrate
   completed experiment → checkpoint → changed allocation, including retry and
   preview invariants. New cycles can add information, but outcomes do not arrive
   every cycle and should not manufacture confidence.
3. Establish capacity: a repeatable native load scenario covering the full cycle,
   contacts, resolved evidence, pending actions and outbox backlog. Bound hot reads
   and recovery paths from observed plans. Preserve audit history while ensuring
   steady-state decisions consume compact state plus new evidence.

## Branch review

`growth/show-helper-community-fan-yield` has no missing production change against
the reviewed main. Useful booking telemetry from
`growth/adaptive-booker-backstop` is already present; its remaining silence-based
relationship escalation was deliberately superseded and should not be restored.
Both old branches were subsequently removed; the current remote was main-only.
Delete merged checkpoint branches promptly and enable GitHub automatic head-branch
deletion when repository-setting access is available.


## Final continuity checkpoint after the main bug-hunt sweep

Reviewed main `a2114862661487606fb25f80d6efbb5651ec5772`, then reconciled
`67c597cc581a52c7fdac1b3cd44f141947be4c0d`. The newer main repairs destination
retry, nested source attribution, decline cooldowns, webhook subscriptions and
video analytics; these changes are preserved. Our migrations follow them as
0389/0390. This is a code audit;
there is no new production audience measurement. The earlier numbers above are
historical, and neither those numbers nor synthetic fixtures set capacity targets.

The metacognition monitor now persists one compact workspace checkpoint after a
completed growth evaluation. Preview projects it without writing. Repeated or
older evaluation timestamps cannot increment the streak; changing the optimized
metric resets comparable assessment history. Reads and writes have time budgets,
and a corrupt record is preserved rather than silently replaced. The progressive
exploration escape now survives cycles. It does not increase evidence confidence,
remove consent/moderator holds or bypass provider capacity.

Strategy learning now owns its observation cursor and atomically saves it with
its posterior using one transaction/connection and a nonblocking writer lock.
Previously a failed strategy save followed by a successful causal save could
permanently skip an outcome; the opposite failure could repeat it. Another
learner's checkpoint no longer controls this consumer. Repeated horizon reads do not rewrite the checkpoint. A newly read Y30
settlement or partial-only reading advances the scan watermark without relearning its earlier Y14
strategy observation; otherwise it would remain in every future delta. Corrupt cursor/state blocks that write and is reported.

The causal model also carries the last measurement actually read, rather than
using its later database save time. This closes the read→save gap: an outcome
arriving in that gap remains newer than the consumed observation cursor. Legacy
checkpoints lacking the field rebuild once under the existing 180-day observation
window, then retain compact state plus deltas. Rebuild cost is not constant and
must be measured; this is not an unlimited archive replay guarantee.

Evidence reads now use distinct prepared SQL for delta, full replay and control
contrast, with bound values. An expression index matches all five timestamps and
the eligible-row predicate. The previous CASE selector could scan 100,000 rows
under a generic prepared plan even for an empty delta. The exact SELECT on a
synthetic PostgreSQL 18.3/PGlite fixture instead used a bitmap index range reading
zero or one new row. Timings are fixture diagnostics, not native latency SLOs.
The per-read duplicate-assignment history scan was removed: migration 0201 already
rejects duplicates and installs the unique workspace/action constraint; the
bounded lateral lookup remains. `verify-learning-scaling.sql` reproduces the
range-plan comparison on disposable temporary data in native PostgreSQL.

Cycle reports now show the configured growth goal, current value, monthly gain,
assessment, learning streak and effective dispatch multiplier. The migrated
regression fixture uses the illustrative Spotify scenario with flat Signal to
verify that a Spotify goal gets its own assessment. Choosing that goal remains
an explicit tenant configuration; the code does not silently change it.

Main #402's CI was failing because the staff-device contract demanded a stale
schema-version comment despite the API deriving the real version from migrations
at compile time. The contract now checks the actual compiler input and builder
rather than a manually maintained comment.

### Validation and remaining acceptance gates

The actual brain/domain Cargo suites ran locally with serialization intact:
After reconciling the newer main: 641 brain tests, 1,288 domain tests,
183 application tests and one doctest. Compact-checkpoint tests cover
100,000 observations, saturation, stale/retry handling, metric reset and preview
projection. The actual SQLx metacognition adapter ran against the PostgreSQL wire
fixture with 50,000 workspace state rows and 100,000 cycle rows. Those are not
50,000 contacts or a full production cycle workload. A separate adapter harness
compiles the actual evidence SELECT/decoder and strategy writer against the real
brain/domain crates; its operator revision writer is a facade. The fault injection
preserves stored state and a fresh process resumes the actual strategy writer
without duplicate learning. The socket proxy has one backend, so this is not a
native concurrency proof. Native migration,
concurrency, lock timeout, Spotify snapshot, independent strategy retry and causal
read/save regression tests are selected explicitly in CI's migrated e2e step.

The remaining P0 is still **live growth proof**. Follow a delivered campaign to
provider receipt, platform-specific destination clicks, subsequent fresh audience
readings and attributed activation/retention. Show the denominator at each step:
requested, eligible, approved, dispatched, actually delivered, clicked and joined.
Correlated Spotify/social gains should be visible successes, while only grounded
attribution or a valid comparison can teach per-action causal effect. A flat
Signal series must not erase another configured platform's gain.

Timestamp cursors still assume observations commit in timestamp order. A very
late transaction with a measurement timestamp older than an already consumed
cursor can be missed; a committed durable horizon inbox/receipt protocol would
close that different race. Do not claim exactly-once learning for arbitrary late
commits from these changes. The immutable audit log is currently best-effort and
is not a substitute for a transactional learning inbox.

Capacity still needs native, mixed-tenant full-cycle evidence: contacts, approvals,
outbox/agent backlog, provider waits and complete/recovery paths; p95/p99 latency,
queue age, lock waits, memory and delivered outreach/hour. The new index and
workspace checkpoint remove demonstrated costs, not every potential bottleneck.
Ordinary index creation blocks writes while building; measure its build duration
and schedule migration before production release. Historical session activation
can still be a lower bound until immutable meaningful-activity receipts exist.
