# Maintenance-grade VIRYA OS rollout

This rollout intentionally makes the production readiness receipt stricter than the previous desired-state check. A temporary red receipt during staged deployment is expected; do not weaken the verifier to make a partial rollout green.

## Order

1. **CrowdRelay API + worker** — deploy the same immutable Git SHA. `crowdrelayctl` records each running container image ID plus `Cargo.lock` SHA-256.
2. **Virya web** — promote the prebuilt Netlify artifact. The ledger receives the Git SHA, package-lock SHA-256 and artifact-manifest content root.
3. **Synesthesia** — promote the exact CI Web artifact and its provenance sidecar.
4. **Virya Signal** — publish the signed APK/AAB production artifact; Google Play production is also a ledger reporter.
5. **n8n** — export the exact live workflows, generate the secretless attestation, run the bound credential/provider smoke and publish a heartbeat generated from that same manifest/attestation pair.
6. Run **Production operational readiness**. Preserve `virya-os-release-receipt.json` with the release/deploy record.

## Acceptance

The receipt is PASS only when all six component records exist and are fresh, CrowdRelay API/worker source SHAs match, every code component has an immutable artifact digest and dependency lock SHA, Web/mobile artifacts have an artifact-manifest content root, n8n manifest/attestation hashes match, and a healthy executor actively advertises `team.email`.

## Team email E2E

Use an existing non-terminal `team.assignment.email` action where possible. Expected path:

`queued -> worker fast lane -> outbox viryaos.team.assignment_email_requested -> n8n execution claim -> Gmail provider -> execution receipt -> succeeded`

Acceptance evidence is: action terminal `succeeded`, execution claim terminal `succeeded`, and a non-empty Gmail provider reference. Replaying the same provider receipt must return replayed/idempotent behavior and must not send a second message.

## Controlled failure test

After the happy-path proof, perform one non-destructive failure exercise in a staging/disposable fixture: claim an emitted action, simulate lost provider response, verify duplicate claim is `in_flight`, verify a claim older than 15 minutes is `ambiguous`, then record success and a delayed failure. The durable claim must remain `succeeded` and all later claims must return `already_succeeded`.

Never run a forced Gmail failure against a real member recipient merely to prove retry behavior.
