# Tracked delivery checkpoint — 2026-10-01

Baseline: main `2b8cfe30c8742dcd119cbbf7b428c62769485e80`. This checkpoint repairs a verified code seam; it is not a diagnosis of the production degraded phase.

## Fresh operational evidence
At approximately 19:43 Europe/Warsaw, API /v1/meta reports 8052c175, schema390, while main has advanced. /metrics reports 288 cycles/24h, 196 degraded, 101 actions created/24h with 51 currently failed, 16 completed evidence rows, 11 activated_fans_30d. Detailed /v1/admin/ops/cycles?state=degraded returns401 in this execution environment. Worker/agents revision and phase-specific warnings remain unverified. Do not infer zero lifetime traffic from reset process-local counters.

## Repaired seam
The outward letter gate accepted any URL beginning with the member-site /l/ prefix. A missing, malformed, inactive, or different-workspace redirect could therefore be emitted as if its CTA were measurable.

The domain scanner now requires an HTTPS member-site origin and valid exact smart-link slug. Dispatch resolves every printed slug against active smart_links in the current workspace and holds shared row locks through the emission transaction. Missing links refuse with an explicit reason; edited text cannot author a destination. The send's existing approval/contact/duplicate/executor gates remain in force. Broadcasts and community-body payloads retain their existing scope.

A refused preflight writes no emission. A trusted redirect restoration lets the same claim execute with the same action/send identity. This is a regression test of the execution boundary, **not** proof that the production worker automatically requeues a terminally refused action.

## Release proof repaired
The outreach-engine fixture now declares a member-site origin and verifies printed tracked CTAs resolve to the intended catalogue/ticket destination. External destination URLs remain the source truth, not text printed in the letter. Archive-wave fixtures explicitly declare fan-origin, and stronger-looking unqualified inbound/multi-source sightings stay excluded.

The previously unselected outward_link_gate PostgreSQL suite is now selected in CI together with the existing outreach/archive proofs. Regressions cover missing, foreign-workspace, inactive and malformed redirects; a live redirect, no-link letter, and restoration of the same claim.

## Validation boundary
This environment has no Cargo/rustc or native PostgreSQL. Native compilation, rustfmt, clippy and the PostgreSQL execution regressions must be verified by repository CI. The PR remains draft until those checks pass. Whitespace checks and scoped source/CI wiring checks are local checks only, not substitute behavioral proof.

No deploy, outreach send or production mutation was performed. No fan uplift is claimed.

## Next acceptance
1. Obtain authorized degraded-phase/action error breakdown and actual component revisions.
2. Finish native CI and deploy the validated artifact under the normal release workflow.
3. Follow a real, permitted publication/delivery through tracked visit, canonical acquisition, maturity, accepted evidence and next choice.
4. Implement measured per-lane recovery/reconciliation where the real failure evidence demonstrates a gap; do not invent retry policy from aggregate failure counts.
