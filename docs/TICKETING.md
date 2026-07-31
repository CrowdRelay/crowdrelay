# First-party ticketing

CrowdRelay owns ticket inventory and admission credentials. Stripe remains the payment authority, while the Virya server verifies Stripe webhook signatures and forwards only normalized, authenticated transitions to CrowdRelay.

## Invariants

- A ticket sale is attached to one future, published event and one admission pool.
- `admission_pools.issued_count + reserved_count <= capacity` is enforced by PostgreSQL.
- Stripe holds, manual passes, weighted-draw prizes and paid tickets therefore cannot oversell the same pool.
- Public clients submit ticket type slugs and quantities only. Price, currency and VAT are always read from CrowdRelay.
- One idempotency key identifies one immutable reservation request.
- One order can be bound to exactly one Stripe Checkout Session.
- New checkouts stop when a complete configured hold no longer fits before `sales_close_at` or the event start.
- Only unbound reservations expire from CrowdRelay's clock. A bound Checkout hold is released by a verified Stripe expiry/failure event, preventing delayed successful webhooks from losing paid inventory.
- A Stripe event ID can be processed repeatedly only with an identical normalized payload.
- A successful payment atomically turns the reservation into claimed `admission_passes`.
- Paid passes use the existing one-time staff redemption endpoint.
- A full refund revokes unused passes. A redeemed pass remains in the audit history and is never made reusable.
- Buying a ticket creates or reuses the fan identity but does not create a marketing consent.

## Inventory semantics

Every sale and ticket type exposes three disjoint counters:

- `sold`: capacity already converted into issued admission passes;
- `reserved`: capacity held by an active reservation or Stripe Checkout Session;
- `available`: capacity still available for a new reservation.

The invariant is `sold + reserved + available = capacity`. A `checkout_created` order is therefore visible as an in-progress payment and reduces availability, but it is not counted as a completed sale. Bound Checkout holds remain reserved until a verified Stripe success, expiry, or failure transition arrives. The admin overview additionally reports checkout order count, reserved ticket units, paid order count, and paid ticket units so UI clients do not have to infer business state from raw order statuses.

## Authentication boundaries

| Routes | Authentication |
|---|---|
| `GET /v1/public/events/{slug}/tickets` | public |
| `POST /v1/public/events/{slug}/ticket-orders` | `Idempotency-Key` |
| `GET /v1/public/ticket-orders/{order_id}` | private checkout bearer token |
| `GET/POST /v1/admin/events/{slug}/ticketing` | admin bearer key |
| `GET /v1/staff/events/{slug}/ticketing` | staff or admin bearer key |
| `/v1/internal/ticket-orders/...` | commerce bearer key |

Admin, staff and commerce credentials are separate. The commerce credential is intentionally shared by the server-only `/commerce/*` and `/internal/*` service namespaces. Namespace middleware rejects cross-role credentials before a handler is entered, while handlers keep their own authorization checks as defense in depth. The commerce key is `CROWDRELAY_COMMERCE_API_KEY`; the same plaintext value must be configured as the server-only `CROWDRELAY_COMMERCE_API_KEY` on the Virya deployment. No privileged credential may be exposed as a public environment variable, embedded in a browser bundle, or shipped inside the fan application.

## Configure a sale

The event must already exist in CrowdRelay, be `published`, start in the future and have a sales close time no later than the event start.

```sh
curl -X POST \
  "https://signal-api.virya.music/v1/admin/events/gig-example/ticketing" \
  -H "Authorization: Bearer $CROWDRELAY_ADMIN_API_KEY" \
  -H "Content-Type: application/json" \
  --data '{
    "currency": "PLN",
    "vat_rate_basis_points": 800,
    "capacity": 250,
    "max_per_order": 8,
    "hold_seconds": 2100,
    "sales_open_at": "2026-08-01T10:00:00Z",
    "sales_close_at": "2026-09-20T16:00:00Z",
    "active": true,
    "ticket_types": [
      {
        "slug": "normalny",
        "name": "Bilet normalny",
        "description": "Wstęp na koncert Virya",
        "price_gross_minor": 5000,
        "capacity": 220,
        "sort_order": 10,
        "active": true
      },
      {
        "slug": "early-bird",
        "name": "Early bird",
        "description": "Limitowana pierwsza pula",
        "price_gross_minor": 4000,
        "capacity": 30,
        "sort_order": 0,
        "active": true
      }
    ]
  }'
```

Money is stored in minor units. `5000` means `50.00 PLN`. The configured price is VAT-inclusive. Order-level net and VAT totals are authoritative; unit fields are informational and may differ by one minor unit after multiplication because totals are rounded from the complete line value.

The generated admission pool slug is `paid-tickets`. Future prize or guest-list configuration that must share the venue capacity should target this same pool rather than create a second physical-capacity pool.

## Stripe event flow

1. Virya reserves capacity in CrowdRelay.
2. Virya creates a Stripe Checkout Session from the server-authoritative order.
3. Virya binds the Stripe Session ID to the reservation.
4. Stripe calls the existing signed Virya webhook.
5. Virya verifies the raw Stripe body, identifies ticket metadata and sends the normalized event through the commerce route.
6. CrowdRelay records the Stripe event and commits payment, release or refund atomically.
7. `ticket.order.paid` or `ticket.order.refund_recorded` is appended to the transactional outbox.

The paid outbox payload contains the buyer, event, accounting totals and static pass references required by the later ticket-mail workflow. It does not grant marketing consent.

## Deployment order

1. Back up PostgreSQL.
2. Deploy and run migration `0011_ticketing.sql` with the CrowdRelay API changes.
3. Configure stable admin, commerce and admission QR signing keys.
4. Configure a sale and verify the public ticket endpoint.
5. Deploy the Virya server routes.
6. Exercise a Stripe test payment, duplicate webhook delivery, expiry and full refund before enabling live sales.

Do not rotate the admission QR signing key or commerce key in the middle of an active checkout rollout without a coordinated deployment.
