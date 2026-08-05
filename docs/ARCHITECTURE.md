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

## Reward campaigns

A physical reward campaign reuses the existing weighted draw and existing
`physical_reward.granted` outbox contract. A new allocation links one draw to a
SKU, reserves `winner_count × units_per_winner`, and creates fulfillment rows
only for actual selected winners. If fewer winners qualify, unused reservation
quantity is released inside the draw transaction.

Campaign states remain the established draw states: draft, scheduled, running,
completed and cancelled. Scheduling is separately gated. Cancellation is only
allowed before running and releases the allocation.

## Public site failure isolation

Static product cards render immediately. Only the small availability indicator
calls `/api/merch/inventory`, with bounded upstream and browser timeouts. A
CrowdRelay outage produces an error/retry state rather than blocking or removing
the page.

## Mobile payment boundary

Virya Signal reads the coarse public catalog and links products to the existing
Virya store. Concert cards continue to use `PublicEvent.ticket_url` and open the
system browser. Both tickets and merch therefore use hosted Stripe Checkout;
the mobile app never embeds card collection or stores payment credentials.

## n8n and mail boundary

No workflow JSON was modified. The existing mail paths continue to consume the
same events. The protected `physical_reward.granted` outbox block and the Stripe
mail/shipment block are covered by exact SHA-256 source guards.

# Store

## Source of truth

CrowdRelay owns products, SKU variants, inventory movements, reservations,
physical reward allocations and fulfillment. The stock value is derived from
an append-only ledger; active reservations are subtracted separately.

`available = sum(inventory_ledger.delta) - active_reservations`

The ledger records receipts, initial counts, sales, refunds, manual adjustments,
promotional issues, damage and staff issues. Every mutation is workspace-scoped
and the externally retried operations use stable idempotency keys.

## Stripe order lifecycle

1. Virya validates the cart and expands bundles to physical SKU quantities.
2. With the site write switch enabled, CrowdRelay creates an expiring order
   reservation before Stripe Checkout is created.
3. The checkout request identity is a fingerprint of the complete cart,
   customer, parcel-locker, language and reward payload. An unchanged retry
   reuses the same Stripe idempotency key.
4. Ambiguous Stripe failures do not release stock immediately. The reservation
   expires naturally, or the exact checkout retries safely.
5. Signed Stripe payment events run the established mail and InPost checkpoints
   first. Inventory commit is appended afterwards.
6. CrowdRelay commit is idempotent. A paid event may also commit an expired or
   released reservation when Stripe events arrived out of order; preserving a
   paid order takes precedence over hiding a temporary negative correction.
