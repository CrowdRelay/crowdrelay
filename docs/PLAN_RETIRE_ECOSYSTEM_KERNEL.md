# Plan: retire the API-side ecosystem mutation kernel

Status: not started. The pattern is proven by the feature-flag slice (`b12e19e`).

## Why

`crates/crowdrelay-api/src/ecosystem.rs` still holds a private transaction
kernel — `mutation_key`, `lock_mutation`, `existing_mutation`, `validate_replay`,
`append_action`, plus `hash_json` / `deterministic_id` / `ExistingMutation`.
`docs/ARCHITECTURE.md` puts multi-row invariants behind a repository; this is an
advisory lock, an idempotency replay window and an audit write living in the
HTTP layer.

Two callers remain, both in `ecosystem/control_plane.rs`:

- `reconcile_inner` (~130 lines: run row, findings, outbox event, audit)
- `update_checklist_inner` (~95 lines)

Moving **both** retires the kernel from `crowdrelay-api` entirely. Moving one
leaves it half-used, which is worse than leaving it whole — do them together.

`API_SQL_RATCHET` is at `writes=140 baseline=140`. This slice is ~12 writes.

## Prerequisites

```bash
make db-up
DSN="postgres://crowdrelay:crowdrelay-local-only@127.0.0.1:5432/crowdrelay"
for v in TEST OUTBOX_TEST ADMISSION_TEST EVENT_TEST AUTOPILOT_TEST \
         FAN_LIFECYCLE_TEST REFERRAL_TEST REMINDER_TEST RETENTION_TEST \
         ACQUISITION_TEST ECOSYSTEM_TEST; do
  export CROWDRELAY_${v}_DATABASE_URL=$DSN
done
```

## Steps

1. **application** — extend `crates/crowdrelay-application/src/ecosystem.rs`:
   `RunReconciliationCommand` -> `ReconciliationOutcome`,
   `UpdateShowChecklistCommand` -> `ChecklistOutcome`, both on
   `EcosystemControlPlaneRepository`. Keep the replay/conflict vocabulary already
   there.

2. **infra** — `crates/crowdrelay-infra/src/ecosystem.rs` already has the kernel
   privately. Generalize it: `validate_replay` currently hardcodes `FLAG_ACTION` /
   `FLAG_TARGET_TYPE`; parameterize on `(action, target_type)`. Then port both
   transactions, SQL verbatim, including `insert_reconciliation_findings` and the
   outbox insert.

3. **api** — `reconcile_inner` / `update_checklist_inner` become thin: validate
   input, call the port, map errors. Then delete the kernel and anything it
   orphans. `cargo check` will name the dead code.

4. **Ratchet** — lower `scripts/api-sql-ratchet.json` for both
   `ecosystem.rs` and `ecosystem/control_plane.rs` to whatever
   `python3 scripts/api-sql-ratchet.py` reports. Never raise it.

5. **Tests** — extend `crates/crowdrelay-infra/tests/ecosystem_postgres.rs` with
   reconcile and checklist replay + conflict cases, matching the flag test's
   shape. Run it **twice in a row** against the same database; the suite must be
   re-runnable (see `515b851` for why).

## Watch for

- **Modularity contract**: `crowdrelay-infra/src/ecosystem.rs` will pass the
  1000-line parent limit. Split with `include!("ecosystem/…rs")` and register the
  chunks in `scripts/test-modularity-contract.py`, as `event_sync.rs` does.
- **Findings queries encode business rules.** Moving the SQL to infra is right;
  whether the *rules* belong in domain is a separate question. Note it, do not
  bundle it in.
- **The declared-flag boundary** documented on the port applies here too: the
  caller owns input policy, the repository owns the transaction.

## Gate before commit

```bash
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-targets --all-features
make contract-tests && make runtime-contracts && make validate-contract-assets
# then every postgres target, twice:
for t in acquisition_postgres admission_postgres events_postgres \
         fan_lifecycle_postgres referrals_rewards_postgres \
         autopilot_team_email_postgres ecosystem_postgres; do
  cargo test --locked -p crowdrelay-infra --test $t -- --ignored --test-threads=1
done
```

Also add the new test target to `ci.yml`'s sequential loop if a new one appears.
