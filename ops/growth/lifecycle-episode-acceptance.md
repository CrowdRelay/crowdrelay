# Autonomous lifecycle episode acceptance

A lifecycle request should answer a real fan moment once. The merged episode foundation (#453, with the ticket-schema correction #455) makes requests stable across unrelated contacts and policy edits. This checkpoint protects it with regressions and closes a retention priority bug: an unpaid fan's old Synesthesia completion previously selected its follow-up forever, ahead of dormancy. The follow-up now stops at the existing policy's dormancy threshold. Recent real attendance prevents reactivation and does not resurrect stale onboarding.

## Expected behaviour

- Five paid orders for the same show count as one paid show. Orders join their sale to resolve the show, scoped to the workspace. Future payments do not count.
- Pending or reversed referral attributions are not qualified referrals. Qualification time, rather than signup acceptance time, anchors the acknowledgement.
- An unrelated touch, interest, check-in or policy version does not create another first-ticket thank-you. A new qualified referral can create a new acknowledgement.
- Historic requests remain authoritative across the identity change, including pending, failed, cancelled and unknown actions. Recovery stays on the original action. Concurrent evaluator variants for one episode persist one action. Existing approval lapse bounds remain in charge of the current key family.
- Attendance can earn an onboarding referral invite within the existing age window and makes a recently attending fan non-dormant.
- A quiz completion at or beyond `dormant_after_days` no longer wins the onboarding branch. A genuinely dormant fan reaches reactivation; a recently attending fan remains quiet until another eligible event.

No policy configuration field or migration is added. Consent gates, approval scopes, contact cooldowns, operator holds and dispatch-time rechecks keep their existing authority.

## Reproduce the offline SQL proof

The validator reads the actual snapshot and compatibility SQL from Rust sources. Ticket sales and orders are created from migration 0011 verbatim, including its column constraints and foreign keys. Other dependencies use reduced fixtures. A negative control restores the invalid `ticket_order.event_id` reference and must fail, so an invented fixture column cannot hide that regression again. This is an offline PostgreSQL/PGlite proof, not a production trace or a Rust compilation check.

```sh
validation_dir=$(mktemp -d)
npm install --prefix "$validation_dir" --no-save @electric-sql/pglite@0.5.8
CROWDRELAY_VALIDATION_NODE_ROOT="$validation_dir" node scripts/check-lifecycle-episodes.mjs
```

Expected: 32 assertions pass. They cover paid-show counting, pending/future qualification, future consent/interest/check-in, tenant isolation, old request states, legacy counters/timestamps/show identity, and current approval families.

## Native regressions

Four domain, two application and four PostgreSQL integration tests accompany the implementation. Integration tests use the migrated disposable test database and the real persistence transaction, including a concurrent race. The existing `postgres` test target includes them via the standing-approval module.

```sh
cargo test -p crowdrelay-domain audience_lifecycle
cargo test -p crowdrelay-application lifecycle_tests
# Set CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL to a disposable migrated database.
cargo test -p crowdrelay-infra --test postgres autopilot_standing_approval -- --ignored
```

At preparation, the SQL proof and Node syntax check passed, changed Rust files parsed/formatted with actual rustfmt WASM, and the diff passed whitespace checks. This environment had no cargo/rustc or native disposable PostgreSQL service; native compilation, clippy and native regression execution were unavailable. CI remains responsible for those checks.

## Organic acceptance after authorized deployment

Follow the next real publication with its action-owned tracked link through a real confirmed signup. Separately verify a real activation event and a qualified referral retain their fan identity and timestamps. Run enough later cycles to prove the same lifecycle moment does not create a second request and a new legitimate moment remains eligible. A queued request or outbox emission alone is not proof of delivery, activation, retention or organic fan growth.

Production `/v1/meta` and `/metrics` reads were unavailable during preparation. No current fan conversion result is inferred from offline tests. The next growth claim requires a live attributable fan outcome after deployment, not a stronger prior or a larger action count.
