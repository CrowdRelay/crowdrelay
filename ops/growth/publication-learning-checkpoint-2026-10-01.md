# Keep learning alive until publication — 2026-10-01

Base: b43186424fc80abb44d93f2967fd40b5a45294a8, after #437.

## Demonstrated gap

fan_windows deferred unpublished content-click/acquisition measurements, but
its five attributed-fan kinds only deferred once a tracked surface was live.
Before publication, a due fan measurement was claimed and its observer returned
no_tracked_link. The terminal abandoned row could never learn from a later post.

The pending-post check also omitted the agent-outcome lineage branch used by
the live-post observer. A child post with an independent trace could therefore
be seen after publication, yet disappear while waiting for that publication.

## Change

All seven publication-dependent kinds retain pending status while a lineage
post is pending, posting, rate-limited or awaiting manual publication.
The bounded claim batch schedules another check in six hours without spending
an attempt. A live surface still gets its original full horizon: three or
fourteen days for acquisition, forty-four days for the complete durable cohort,
and seven days from publication itself for content outcomes.

The pending and live checks now share the same lineage definition in both
task-enabled and taskless variants. A contract regression compares those CTEs,
including workspace isolation. Failed posts and unrelated pending posts do
not keep a measurement alive; the observer still refuses no_tracked_link.

## Acceptance and validation

New PostgreSQL regressions cover:

- all five fan horizons across all four publishable states, zero attempts,
  six-hour deferral, and a genuinely ready sibling claimed normally;
- a parent whose independent-trace outcome child is awaiting publication,
  then goes live and acquires a fan: wait the full window, claim once, observe
  exactly that attributed fan;
- a failed outcome-child post plus an unrelated pending post: no deferral,
  no invented fan outcome.

CI now selects the entire attributed-fan and window suites, including their
existing attribution, canonical retention and content-window regressions.
The seven Python attributed-fan contracts passed locally. Deliberately
removing the task workspace predicate made the lineage contract fail; restoring
the source made all seven pass again.

Changed-file whitespace and source-size checks passed. This environment has
no native Rust or PostgreSQL toolchain. cargo compilation/tests, rustfmt,
clippy, PostgreSQL regressions and complete repository policy gates remain
unverified locally; the PR is a draft pending native checks.

## Scope and remaining acceptance

No migration, production write, outreach, merge or deploy. Consent, approval,
contact, community and booker rules are unchanged. A manual publication remains
manual; this changes measurement scheduling, not publishing authority.
Rows already terminally abandoned are not reopened. Their evidence/readiness
state needs a separately verified reconciliation before historical repair.

The known cross-template parent/child credit limitation is also unchanged.
This checkpoint prevents losing a future learning opportunity; it does not
establish causal fan uplift or prove a live production conversion.

Read-only production still reports API 8052c175/schema390, with 196 degraded
cycles and 52 failed actions in the trailing 24 hours. Actual degraded phase
causes remain unverified because detailed ops needs an authorized read.
Live acceptance remains the next real tracked publication followed through
provider receipt, canonical acquisition and meaningful activation/retention.
