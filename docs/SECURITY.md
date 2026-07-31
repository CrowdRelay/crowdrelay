# Security

## Secrets

Production secrets belong in the protected production environment or read-only secret files, never in bootstrap JSON or the frontend. Rotate admin, staff, QR-signing, webhook and database credentials independently when possible.

Concert tokens derive a dedicated HMAC key from the admission QR root using domain separation. A concert token cannot be replayed as an admission-pass QR. Campaign state in PostgreSQL remains authoritative, so a signed token can still be revoked.

## Operational controls

- keep `CROWDRELAY_RANDOM_DRAWS_ENABLED=false` until rules, dates and inventory are approved;
- use short concert campaign windows and revoke leaked prints;
- restrict GHCR package write access to the repository workflow;
- back up PostgreSQL before migrations and periodically test restore;
- expose only the public API through Caddy; PostgreSQL stays on the private Compose network.

## Required tests

Changes should pass format, Clippy with warnings denied, unit tests, sequential PostgreSQL integration tests, dependency audit, OpenAPI/client validation, Compose rendering and image builds. Security-sensitive flows also need tests for tampered tokens, expiry, revocation, duplicate check-in, capacity races and cross-event use.
