# Architecture

CrowdRelay is split into a synchronous API and an asynchronous worker around one PostgreSQL database.

## Runtime boundaries

- `crowdrelay-api` owns HTTP validation, fan sessions, public event reads, event interest, concert check-ins, passes and staff/admin operations.
- `crowdrelay-worker` owns bootstrap, transactional-outbox delivery, reminders, retention, weighted draws and external event synchronization.
- PostgreSQL is the source of truth. Public caches are replaceable snapshots; rewards, check-ins and draw runs are transactional records.
- Virya never exposes a CrowdRelay admin key to the browser. Netlify server routes proxy the small staff-only surface.

## Reliability model

Public page requests never call Bandsintown through CrowdRelay synchronously. The worker fetches and validates the feed outside a database transaction, then applies a bounded upsert. Failed or suspicious empty imports retain the previous event snapshot.

Reward delivery is based on a transactional outbox. Draw candidates and weights are snapshotted before winners are issued. Concert check-in capacity is serialized by a row lock and one fan can check in only once per event.

## Trust boundaries

- browser → public API: untrusted JSON, bounded body, fan session;
- Virya server → admin API: bearer key, no browser access;
- worker → provider/webhook: timeouts, bounded payloads and signed delivery;
- staff scanner/QR panel: separate operational sessions and revocable server-side state.

## Build graph

The Docker build uses a pinned `cargo-chef` dependency recipe and compiles API plus worker in one Cargo invocation. Buildx Bake exports one shared GHA cache, so ordinary source-only commits reuse the dependency layer and do not compile the workspace twice.
