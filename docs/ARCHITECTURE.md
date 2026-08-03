# CrowdRelay architecture

CrowdRelay is a PostgreSQL-authoritative, event-driven backend for artist and event operations. The public request path commits durable state first; provider delivery and automation happen asynchronously.

## Workspace layout

| Crate | Responsibility |
|---|---|
| `crowdrelay-domain` | identifiers, events and business value objects without infrastructure |
| `crowdrelay-application` | use cases and repository ports |
| `crowdrelay-infra` | PostgreSQL repositories, configuration, caches and observability |
| `crowdrelay-api` | HTTP authorization boundaries, validation and response contracts |
| `crowdrelay-worker` | outbox delivery, reminders, retention, event sync and weighted draws |

The split is intentionally layered. New vertical slices should keep policy in domain/application code and isolate SQL, HTTP and provider details in infrastructure or adapters.

## Consistency model

- PostgreSQL is authoritative for fans, consent, tickets, admission, accounting and outbox state.
- Multi-row business invariants are committed in one transaction.
- Retryable writes require an idempotency key and a payload-compatible replay.
- Provider delivery is at-least-once. Consumers must deduplicate by event or operation identity.
- Public reads may use bounded caches; private capability and operator reads are `no-store`.
- n8n, email and external proof systems never participate in a public transaction commit.

## Main event path

```text
HTTP command
  -> authorization and bounded input validation
  -> application use case
  -> PostgreSQL transaction
       business rows
       idempotency result
       outbox event
  -> response
  -> worker lease
  -> signed webhook delivery
  -> retry / delivered / dead state
```

## Authorization boundaries

`/public`, `/me`, `/admin`, `/staff`, `/commerce` and `/internal` are separate capabilities. A credential valid for one boundary is not treated as a general system credential.

## Scaling boundaries

The API is stateless apart from process-local read caches. Workers coordinate through PostgreSQL leases. Horizontal scale therefore depends on database contention, endpoint fairness and bounded concurrency rather than process affinity.

## Deliberate trade-offs

- PostgreSQL is preferred over an additional broker while throughput and retention remain within measured limits.
- The transactional outbox is more operationally involved than direct webhooks, but prevents a committed business operation from losing its delivery intent.
- External Rekor proofs are optional evidence, not consensus and not an authority for business state.
- Partitioning and additional infrastructure are deferred until production measurements justify them.
