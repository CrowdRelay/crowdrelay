# Autonomous growth from tenant operations

## Checkpoint base

The original draft checkpoint was based on `3041f153`. This revision merges main at `7d6730b2` without rewriting published branch history. It keeps main's canonical promotion migration at `0384_source_owned_promotion_metadata.sql` and removes the checkpoint's obsolete rename to `0383`. The shared unsent-state predicate preserves main's SCOUT send guard and applies the same refusal to contradictory reply fields. The content-supply snapshot retains main's acquired-fan resonance signal alongside the checkpoint's failed-task receipts.

## Goal

Carry the tenant's real videos, releases, and posts to relevant audiences without requiring someone to restart failed internal work. Measure arrival, activation, and retention; do not equate a dispatched task with a published post or a new fan.

## Current path

The scheduled autopilot reads `content_sources`, creates source-specific distribution actions, and dispatches community drafts. Agent outcomes enter the existing approval and relay batch pipeline. Executors enforce consent, platform rules, standing authority, pacing, and send-time source exclusions. Attribution and retention already have owners; this work does not replace them.

## First slice: reliable community preparation

Code inspection found three gaps in the existing drop and relay path:

1. Community selection rotates only on `community_posts`. A dispatched draft that has not produced a post does not consume a turn, so the same initial targets can monopolize the bounded selection.
2. Drop retries aggregate every community into one failure counter. One failed target changes successful targets' keys or exhausts the entire community lane.
3. Creating an agent task marks its action successful. A later failed task is invisible to the drop retry counter. The counter also records a failure timestamp but does not enforce a retry delay.

Acceptance:

- Dispatched source-bound community drafts consume a rotation turn before a post exists. Failed-only work remains eligible for retry.
- Retry accounting uses the existing source and target keys, never a shared community counter.
- A failed agent task contributes one failure even when its dispatch action succeeded. Missing agent tables do not break a CrowdRelay-only deployment.
- Retry waits 30 minutes after the first failure and 60 minutes after the second. The existing attempt ceiling remains unchanged.
- A failed target changes only that target's retry key. Other targets and owned channels retain their keys.
- Prove the behavior with unit tests and disposable PostgreSQL tests, including tenant isolation and duplicate failure accounting.

No external publication, email, push, deployment, or production data mutation is part of verification. Existing approval, consent, authority, and batch gates remain intact.

## Migration prerequisite found during verification

The original checkpoint found two migrations numbered `0382`: a fresh PostgreSQL run failed with `_sqlx_migrations_pkey`, and subsequent starts failed with `VersionMismatch(382)`. Main has since repaired that sequence. The merged branch keeps activation integrity at `0382`, attendance at `0383`, promotion metadata at `0384`, and content acquisition at `0385`. No migration SQL body changes or additional migration are introduced by this PR relative to main.

Before deployment, inspect the target's `_sqlx_migrations` description and checksum if it may have applied the old promotion branch as `382`. Such an installation needs a separately reviewed ledger reconciliation; do not edit its migration ledger or force this rollout. SQLx validates the embedded migration set, so rollback requires a reviewed compatible image or the repository's verified database restoration procedure. No schema contraction, production migration, or rollback is performed here.

## Operation history prerequisite

The full workspace test gate exposed another existing bug: SCOUT's `Date` cell made a `NOT SENT` row read as sent, despite its status being normalized to `DRAFT`. That fabricated history can suppress real outreach. Explicit unsent states now veto both send and reply classification even when contradictory dates or reply fields exist. Raw cells remain available for audit, stable row identities do not change, and proven sends retain their existing behavior. No previously imported production history is rewritten by this patch. The SCOUT test and a new contradictory-input test fail before the fix and pass after it. Existing SCOUT tests also use array slices rather than temporary vectors to satisfy the unchanged clippy gate.

## Verification and release status

The focused unit tests pass. Disposable PostgreSQL tests prove audience rotation before post creation, task-failure receipt handling, tenant isolation, retry deduplication, and the missing-agent-schema path. Mutation runs fail when rotation, receipt classification, destination isolation, or retry delays regress. The new PostgreSQL tests are included in `autopilot_relay_loader`, which the existing local and CI recipes already select.

Original checkpoint verification, before merging main:

- Formatting, strict workspace clippy, and workspace Rust tests pass within `just ci`.
- `just test-postgres` passes on the final code.
- `git diff --check` passes.
- `just ci` fails in contract tests: local `CLAUDE.md` counts have drifted, and the fan-consent contract still asserts `max(latest.recorded_at)` against the current metrics implementation.
- `just policy-checks` passes the SQL and architecture ratchets, then fails because the staff-device contract requires the literal marker `SCHEMA_VERSION: u32 = 383`. Recipes after that failure do not run.

The original failing gates were not relaxed. Main now contains the consent and schema-marker contract updates.

Fresh verification after merging main at `7d6730b2`:

- `just check` passes: formatting, strict workspace clippy, and workspace Rust tests.
- `just test-postgres` passes against disposable databases, including the new rotation and task-receipt proofs.
- Migration uniqueness, fan-activation contracts, source-size and workspace-scope ratchets, SQL identifiers, and SQL typed-parameter checks pass.
- The resolved merge and the PR diff both pass whitespace checks. No unmerged index entries remain.
- Full `just ci` is not rerun locally for this conflict-resolution task. The PR remains a draft pending full release verification.

No deployment, tenant publication, human outreach, or historical data repair is performed.

## Follow-up slices, not implemented here

- Audit operation ingestion and campaign coverage for real shows and releases. Repair missing receipts before adding new distribution channels.
- Evaluate campaign outcomes only after the retention window matures. Use actual activated and retained fans to choose the next audience, not follower counts or assumed yield.
- Expose blocked work and its owner on existing operator surfaces. Do not add another dashboard or another approval queue.

These require their own observed failures and acceptance tests. Passing this slice does not establish live growth or prove that any campaign gains fans.
