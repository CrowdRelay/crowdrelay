# n8n integration

CrowdRelay emits durable, HMAC-signed webhook events through its transactional outbox. n8n is an optional execution adapter for external providers; it does not own business state, recipient selection, policy, authority or durable business correlation.

## Public examples

Provider-neutral examples live under [`examples/`](./examples/). They demonstrate the integration boundary without production credentials, domains or operator configuration.

## Execution rules

- Verify the signed event before invoking a workflow.
- Use the CrowdRelay event ID or supplied business key as the idempotency key.
- Claim non-idempotent actions through the CrowdRelay execution-claim endpoint before making the provider call.
- Treat `in_flight` and `ambiguous` results as fail-closed; never resend automatically.
- Report provider outcomes back to CrowdRelay as execution receipts.
- Do not rescore leads, change policy, infer authority or invent alternative actions in n8n.

## Production exports

Production workflow exports are intentionally not stored in this public repository. Credentials, provider IDs, workflow mappings, domains and deployment-specific routing remain operator-local.

For sensitive workflows, disable successful, failed and manual execution-data persistence unless a deliberately redacted audit trail is required.
