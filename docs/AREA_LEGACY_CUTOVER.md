# AREA legacy import cutover

CrowdRelay is the canonical AREA claim/wallet authority. The two website-ledger
import routes exist only as a compatibility bridge and are protected by
`area_legacy_imports_enabled` (default **true**). A flag-read failure fails closed.

## Production cutover gate

1. Record baselines for the **applied** counters `crowdrelay_legacy_area_claim_import_total` and
   `crowdrelay_legacy_area_wallet_import_total`. The separate `*_attempt_total` counters are diagnostic only.
2. Require **at least 14 consecutive days** and at least one representative
   traffic window with zero increase in both applied counters. Invalid requests and idempotent replays must not reset this clock.
3. Reconcile known migrated players/wallet balances/claims between the legacy
   ledger and canonical PostgreSQL state.
4. Verify a supported website build handles HTTP 410
   `AREA_LEGACY_IMPORTS_DISABLED` by continuing from canonical state.
5. Set `area_legacy_imports_enabled=false` with an audited feature-flag update.
6. Smoke-test AREA wallet, challenge/claim, reward, and website reload flows.
7. Observe error rates/counters through a normal traffic window. Roll back the
   flag only if a supported client still needs the bridge.
8. Delete the import code only after the supported-client floor has passed.

Do not flip the flag during a show or on the same change window as a database
restore/migration rehearsal.
