# Growth review — 2026-09-30

## Verdict

CrowdRelay has substantial execution, attribution and learning machinery. It is
not yet a proven audience-growth product. A successful action, a generated post,
an imported contact or another brain cycle is not a new listener or fan. The
critical missing proof is a repeatable live path from system action to delivered
exposure, audience gain and a subsequent useful interaction.

Success includes Spotify followers and audiences on other socials. A verified
Spotify move from 185 to 300 is a meaningful outcome even if Signal stays flat.
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
| P1 | Metacognition does not accumulate its streak. The loader constructs a fresh monitor each snapshot. `learning_cycles` therefore stays at 0/1; `exploration_boost()` includes a progressive escape bonus based on that counter, despite the loader comment saying consumers depend only on assessed state. | Persist state after completed cycles, never during preview. Verify preview is read-only, retry is idempotent, workspace state is isolated and the streak changes exploration as intended without duplicate evidence learning. This checkpoint records the issue; it does not claim to fix it. |
| P1 | Durable causal learning exists but live improvement is unproven. Checkpoint/delta replay, per-horizon cursors and randomized contrasts have dedicated code and regression tests. They do not prove that production experiments resolve or influence the next dispatch. | Trace one resolved experiment through its evidence cursor, model checkpoint hash, changed posterior and next selected action. Verify one evidence horizon is learned once; randomized control remains available across batches; missing/corrupt checkpoint recovery has a bounded cost. |

## Implemented checkpoint and measured scope

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
Both old branches can be removed without cherry-picking their remaining diffs.
Delete merged checkpoint branches promptly and enable GitHub automatic head-branch
deletion when repository-setting access is available.
