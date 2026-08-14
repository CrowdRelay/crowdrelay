# VIRYA OS incident runbooks

These runbooks are deliberately evidence-first and fail-closed. Never repair an incident by disabling claim-before-provider, receipt validation, capability checks, immutable artifact verification, or idempotency. Capture the release receipt and affected IDs before changing state.

## First five minutes

1. Save the current `virya-os-release-receipt.json`, deployed Git SHA/image digest and the affected action/request IDs.
2. Check `/health/ready` and the staff Ops overview before restarting anything.
3. Identify the failing boundary: CrowdRelay queue/claim, outbox/bridge, n8n execution claim, provider, or receipt callback.
4. Prefer replaying the durable action/receipt path over creating a replacement business action.
5. Roll back only to an immutable previously proven artifact; never rebuild an old commit during an incident.

## `team.email` queued / no claim

**Signal:** assignment email action stays `queued`, has no execution claim and no provider reference.

- Confirm `crowdrelay-worker` is healthy and the team-email fast lane is running independently of autonomous Autopilot.
- Confirm the action is `team.assignment.email`, `available_at <= now()` and not exhausted/cancelled.
- Confirm production readiness reports a fresh matching n8n attestation and a live `team.email` executor capability.
- Do not modify n8n/bridge when the action has not reached the outbox.
- After deploying a worker fix, allow the existing durable action to be claimed; do not manufacture a duplicate fixture unless the original is terminal.

**Expected terminal proof:** CrowdRelay action `succeeded`, n8n execution claim `succeeded`, provider reference present.

## Outbox backlog / bridge lag

**Signal:** CrowdRelay action emitted successfully but `viryaos.*` outbox rows accumulate or n8n sees no event.

- Compare outbox oldest age, dispatcher task health and bridge ingress health.
- Verify the exact event type is present in the attested route manifest.
- A worker/API restart is acceptable only after confirming transactional outbox rows are durable.
- Never delete backlog rows to make the dashboard green. Drain or quarantine with evidence.

## Provider ambiguity / worker restart after claim

**Signal:** claim is `claimed`, no terminal provider receipt, process restarted or provider response was lost.

- For the first 15 minutes the executor must return `in_flight`; after that it must return `ambiguous`, not silently re-send.
- Resolve provider state using provider-native correlation where available.
- A `succeeded` claim is monotonic: delayed `failed` receipts are audit evidence and must never downgrade it or grant a new claim.
- Only a confirmed failed terminal attempt may receive a fresh claim token/attempt number.

## Duplicate event / duplicate provider callback

- Re-deliver the same receipt key only when testing idempotency; it must return `replayed=true` and create no second side effect.
- A receipt with the same key but different action/executor/status is a conflict, not a merge.
- Staff UI mutations carry a browser-owned operation ID. Ambiguous timeout/5xx retries reuse it; a confirmed later operation gets a fresh ID. Immutable-intent endpoints may additionally derive a stable server fallback. Never mint a new operation ID just to bypass an ambiguous result or conflict.

## n8n attestation or manifest drift

**Signal:** desired capability enabled but `team_email_live=false`, `executor_manifest_drift=true`, or production readiness fails attestation.

1. Export the exact live workflow set without secrets.
2. Generate and validate the workflow attestation against the current route manifest.
3. Run the bound credential/provider smoke for the exact workflow hash.
4. Publish heartbeat built from that same manifest + attestation.
5. Require `production-readiness` PASS and preserve the receipt artifact.

Never hand-edit heartbeat metadata to force a green capability.

## Worker/API critical task died

- Treat a critical background task exiting unexpectedly as process-unhealthy even if HTTP handlers still respond.
- Let supervision trigger bounded graceful shutdown and restart via the deployment supervisor.
- Inspect the task-specific error before scaling/restarting repeatedly; repeated crash loops can amplify provider or DB load.

## Database outage / pool pressure

- Separate edge/Caddy latency from DB acquire and application handler time using Server-Timing and pool size/idle/max metrics.
- During an outage, prefer backpressure and explicit 503 over increasing pool size blindly.
- Verify migrations and DB health before retrying provider-facing jobs; transactional state is the source of truth.

## Accounting finalization

- Finalize is permitted only for the exact `loadedMonth`/preview currently displayed.
- If the month selector changes, reload preview before finalizing.
- On timeout/5xx, keep the same form intent and retry through the staff UI so it reuses the pending operation ID rather than creating a second document.
- Preserve generated document ID and source period in incident evidence.

## Ticket / admission mutation failure

- Issue/revoke/ticketing writes are same-origin, authenticated and idempotent by validated intent.
- On an ambiguous response, retry the same intent; the pending browser operation ID is reused for up to the recovery window. Do not alter fields merely to obtain a new operation ID.
- Verify upstream state before any compensating operation.

## Stripe / commerce sync

- Distinguish webhook backlog, Stripe API failure and local projection lag.
- Never repair by editing payment/ticket state directly without a corresponding provider fact.
- Reconciliation is a deliberately fresh operator command; retain its operation ID and result.

## Virya Signal compatibility / WASM regression

- Hard build limit remains 1536 KiB; 1400 KiB is the early-warning boundary.
- Compare the Web metrics artifact to the previous successful main baseline before raising a limit.
- `proc-macro-error2` future-incompatibility debt is bounded by `security/future-incompat-budget.json`; resolve/update upstream rather than extending silently.
- Roll back to a signed/released artifact whose provenance sidecar matches the release ledger.

## Synesthesia runtime/deploy rollback

- Loader threaded requests have a deadline and must fail visibly instead of hanging forever.
- Use the CI-proven Web artifact; Netlify deployment must not rebuild source.
- If startup/menu/readability regresses, compare Web metrics and canonical `validate-fast.sh` results, then promote the last proven artifact.

## Performance regression

- CrowdRelay nightly benchmark uses three samples, median aggregation and relative baseline thresholds; do not optimize from a single noisy edge sample.
- Virya, Signal and Synesthesia compare build metrics against the previous successful main artifact with a noise floor.
- Investigate the largest changed dimension first; do not raise a ratchet until the increase is understood and intentional.

## Recovery closeout

An incident is closed only when:

- the business action has an unambiguous terminal state,
- provider reference/receipt exists when an external provider was involved,
- production readiness is PASS,
- the release receipt identifies immutable source/artifact/lock hashes,
- any compensating action is recorded,
- the regression test reproducing the incident is in CI.
