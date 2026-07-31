# Virya ecosystem integration

CrowdRelay is the durable source of truth for fans, consent, events, ticket inventory, admission passes, draws and accounting evidence. Virya owns presentation and first-party Stripe verification. n8n remains a delivery adapter after a signed event is durably accepted.

## Event lifecycle

`Bandsintown → event sync → PostgreSQL → transactional outbox → signed ingress → channel branch`

The first successful Bandsintown sync is a silent backfill. A later newly inserted future event independently queues:

- `event.copy.enrichment_requested` for a facts-only Gemini candidate;
- `event.published` for the Facebook Page post;
- `event.discord_report_due` for the Discord operational report;
- one `event.announcement_due` per eligible regional fan.

Facebook and Discord have distinct outbox events. Failure of one channel cannot block another. Event updates and cancellations use fingerprinted events and notify interested fans plus paid ticket buyers.

## Ticket and reward lifecycle

All paid, draw, referral, AREA and guest-list admissions become `admission_passes` and use the same check-in endpoint. Capacity is protected by one shared `admission_pools` counter. Stripe reservations increase `reserved_count`; successful fulfilment atomically transfers it to `issued_count`.

## Accounting

Stripe events append immutable sale/refund ledger rows. Monthly WEW documents are immutable snapshots and exclude orders requesting individual invoices. Stripe fee/net fields are reconciliation data, not a reduction of gross ticket revenue. The CSV is a semicolon-delimited operational export; confirm field mapping in the current Saldeo configuration before importing.

## Performance decisions

- recipient fanout uses one `INSERT … SELECT` rather than one query per fan;
- outbox materialization is set-based per leased batch;
- delivery is bounded and parallel through `FOR UPDATE SKIP LOCKED`;
- marketing consent is checked again immediately before delivery;
- paid admission passes use one `INSERT … generate_series` per order;
- source descriptions and AI candidates are content-addressed to avoid repeated generation;
- public live-event reads use CrowdRelay first with bounded provider and curated fallbacks.

The exact production dispatcher map and provider-specific workflow exports are operator configuration and are intentionally kept outside the public repository. See `n8n/README.md` for the sanitized integration example.
