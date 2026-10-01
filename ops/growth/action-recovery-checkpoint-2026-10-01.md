# Action recovery checkpoint — 2026-10-01

Base: c532333f03a62f14d67d195fe87a610e04927348, after #434 and #435.

## Demonstrated failure seam

The claim path automatically takes over stale processing actions after fifteen
minutes (two hours for agent runs), with at most five attempts. Before this change:

- execute_action did not check the attempt generation before executing payloads;
- its terminal update checked processing status, but not the attempt generation;
- fail_action accepted only the action identity and could close a newer attempt;
- a reclaimed processing attempt remained started forever in the attempt ledger;
- every retryable failure used the same five-minute delay.

A paused worker could resume after another worker reclaimed its action, commit
the old execution against the new processing row, or mark the newer attempt
failed. Retrying autonomously needs a durable boundary between these attempts.

## Change and acceptance

Execution locks the action row and verifies workspace, processing status and
attempt number before entering any payload branch. The row stays locked through
the existing transaction, so takeover cannot overlap committed execution.
Late failures carry their original attempt number and do nothing if that attempt
is no longer current. Reclaim closes the interrupted attempt as failed with
stale_claim_recovered in the same transaction that starts the next attempt.
Retryable failures wait 5/10/20/40 minutes; attempt five is terminal.

Four PostgreSQL regressions use real approved booking action claims:

1. Take over a stale claim, refuse old execution without emission, ignore its
   late failure and a foreign tenant's writes, execute the current claim once.
   Assert one failed old attempt and one successful current attempt.
2. Verify each retry due time, no early claim, duplicate failure idempotence,
   five-attempt exhaustion and no outward emission.
3. Lose every worker claim; assert all five attempts close, the fifth action
   fails as stale_retry_exhausted, and subsequent sweeps cannot reclaim it.
4. Preserve permanent refusals: no automatic resurrection or stale execution.

The recovery suite is selected in CI's ignored PostgreSQL integration run.
The no-agent-service fixture now supplies a real processing action row so it
still reaches the service-absence guard instead of failing the new claim guard.
The attention-budget failure helper supplies the original first attempt.

## Validation and operational limits

Local changed-file whitespace, source-size allowance, failure-call wiring and
execution/claim ordering inspections passed. This environment has no cargo,
rustc, rustfmt or PostgreSQL. Native Rust compilation, formatting, clippy,
PostgreSQL regressions and full-checkout policy checks remain unverified here.
Main CI was queued when checked; skipped alert workflows are not test success.

Read-only production refresh still reports API 8052c175/schema390. Metrics report
288 cycles, 196 degraded cycles and 52 failed actions in the trailing 24 hours.
GET /v1/admin/ops/cycles?state=degraded returns 401 without an authorized ops read.
These facts do not establish the actual phase-specific production cause.

No migration, provider wire-format change, outreach, merge or deploy is included.
Approval, consent, contact cooldown, executor capability, community and booker
hold gates remain on their existing paths. Terminal business refusals stay
terminal. This is recovery of the transactional action-dispatch claim, not
automatic replay of an ambiguous external provider delivery.

## North Star

Reliable takeover avoids duplicate or stale dispatch and gives the Brain an
honest execution reliability ledger. Delivery and provider receipts still have
to produce live tracked surfaces and real canonical fans, then meaningful
activation, retention and qualified referrals. This checkpoint establishes no
fan uplift and does not close the degraded-evaluation diagnosis or live-trace
acceptance.
