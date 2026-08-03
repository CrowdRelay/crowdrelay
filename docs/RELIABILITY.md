# Reliability and failure model

## Process failures

The API and worker install a process-wide panic hook after structured tracing starts. A panic emits a bounded structured error with service name and source location before normal process supervision restarts the container. Runtime task failures are still propagated through `JoinHandle` results and are not silently converted into success.

## Durable delivery

Outbox and webhook rows move through bounded states with leases, attempt counters and terminal dead state. A worker death before finalization leaves recoverable durable work. Manual retries are owner-only, idempotent and audited.

## Backpressure

Public analytics use bounded in-memory buffers. Delivery workers use finite batch sizes, request deadlines and concurrency limits. Queue age, pending/dead counts and database health are exposed operationally; no unbounded channel is part of the request path.

## Degraded operation

- failed cache refreshes retain the previous snapshot;
- Signal control-plane secondary city aggregation can degrade independently;
- external webhook, n8n and proof outages do not roll back committed fan, ticket or admission state;
- graceful shutdown has a deadline and aborts tasks that cannot finish in time.

## Recovery evidence

A useful incident report includes service name, image SHA, request/correlation ID, operation or event ID, queue state and the first failing dependency. Logs must not contain bearer tokens, raw webhook payloads, e-mail addresses or ticket capabilities.

## Verification gates

```sh
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
python3 scripts/test_portfolio_reliability_v14.py
```

## Known limits

A process can be terminated by the kernel before userspace writes another log line. Container restart policy and host-level logs remain required. The panic hook improves evidence for Rust panics; it does not claim to intercept OOM kills, host failure or forced termination.
