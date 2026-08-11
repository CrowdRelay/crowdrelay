# CrowdRelay stable integration contract — 1.x

## Source of truth

The stable cross-team contract is `openapi/openapi.yaml`. Internal Rust crates are layered implementation modules and are not the compatibility boundary.

### Stable surfaces

- `/v1/public/*`, `/v1/me/*`, `/v1/admin/*`, `/v1/staff/*`, `/v1/commerce/*`, `/v1/internal/*` authorization namespaces;
- typed request/response schemas in OpenAPI;
- signed webhook envelope version and event type names;
- idempotency-key behavior for retryable commands;
- ViryaOS executor capability heartbeat and `accepted|executing|succeeded|failed` execution receipts;
- release-component ledger and first-party RUM ingress.

### Private surfaces

- SQL schema and migration layout;
- crate/module names and repository traits;
- internal queue/lease implementation;
- provider choice (n8n, Gmail, Calendar, Discord, etc.);
- control-plane query implementation.

## Semver policy

- **PATCH:** bug/security/performance fixes that preserve observable contract behavior.
- **MINOR:** additive endpoints, fields, enum values where clients are required to tolerate unknown additive data.
- **MAJOR:** removal/rename, incompatible validation/auth changes, or event semantic changes.

## Architecture invariant

`domain -> application -> infrastructure -> transport/worker` remains the dependency direction. A consumer requirement does not justify moving provider or SQL knowledge into the domain model.
