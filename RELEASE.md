# CrowdRelay 1.0.0

CrowdRelay 1.0.0 is the stable service-contract release of the backend and ViryaOS operations plane.

## 1.x compatibility promise

- `openapi/openapi.yaml` is the canonical HTTP contract and follows semantic versioning.
- Existing 1.x request/response fields and event envelope semantics are additive-only unless a documented security fix requires stricter validation.
- Signed webhook event names, idempotency semantics and executor receipt states remain stable within 1.x.
- Database schema and internal Rust modules are implementation details; consumers must not couple to them.
- Business policy stays in domain/application code. Provider orchestration stays outside the public transaction path.

## Reuse model

Other teams should integrate through the service contract rather than copy the internal crates:

1. Generate a client from `openapi/openapi.yaml` in the consumer language.
2. Consume signed webhook envelopes and deduplicate by event/operation identity.
3. For external executors, advertise explicit capabilities and return execution receipts.
4. Treat PostgreSQL, internal crate topology and n8n workflows as private implementation details.

This keeps the DDD boundaries reusable without forcing downstream teams onto Rust or CrowdRelay's internal persistence model.
