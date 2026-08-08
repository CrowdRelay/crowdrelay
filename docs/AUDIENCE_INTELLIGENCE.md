# Audience Intelligence

Audience Intelligence is CrowdRelay's first-party read and orchestration plane for the fan relationship. It does not replace acquisition, consent, ticketing, admission, commerce, n8n or provider delivery. PostgreSQL remains authoritative and no provider call is added to an HTTP request path.

## Scope

The slice adds four bounded capabilities:

1. **Fan 360** — admin-only fan cards and detail views combining acquisition, referrals, event interest, attendance, tickets, rewards, Synesthesia and operator tags.
2. **Reusable segments** — declarative JSON filters evaluated against canonical CrowdRelay state. No fan PII is copied into a segment table.
3. **Communication intent** — draft/scheduled/completed/cancelled campaign metadata. Scheduling is feature-gated and emits only a non-PII `communication.campaign_due` outbox event.
4. **Analytics** — acquisition funnel and currency-safe ticket revenue summaries. Monetary values from different currencies are never summed together.

## Safety boundaries

- `communication_campaigns_enabled` defaults to **false**. Enable it only after the downstream event adapter knows how to handle `communication.campaign_due`.
- Recipient **membership** is frozen once, on the first delivery-plan request, as fan IDs only. The snapshot stores no copied e-mail/name PII.
- Every delivery-plan page joins the current fan record and rechecks active status plus the latest `marketing` consent, so unsubscribe/suppression still takes effect after the membership snapshot was created.
- Email delivery also fails closed while `mailer_enabled` is disabled.
- Scheduling does not contain recipients, e-mail addresses or message bodies in the outbox payload. The event contains only campaign/segment/template identifiers.
- The downstream adapter must fetch the internal delivery plan after the event becomes due. A cancelled campaign returns a conflict instead of a recipient list. Delivery plans are cursor-paginated and never silently truncate a large segment.
- Provider delivery remains outside the public/admin transaction. Existing transactional mail flow is unchanged.

## Segment filter

A segment stores an object with any combination of:

```json
{
  "statuses": ["active"],
  "city_slugs": ["wroclaw"],
  "min_qualified_referrals": 2,
  "interested_event_slugs": ["gorzow-guest-list-2026"],
  "attended_event_slugs": [],
  "purchased_event_slugs": [],
  "synesthesia_completed": true,
  "marketing_consent": true,
  "tags_all": ["ambassador"]
}
```

Empty arrays mean “no constraint”. `tags_all` requires every listed tag. Boolean fields may be omitted.

## API

Admin reads and writes:

- `GET /v1/admin/audience/overview`
- `GET /v1/admin/audience/fans`
- `GET /v1/admin/audience/fans/{fan_id}`
- `POST /v1/admin/audience/fans/{fan_id}/tags`
- `POST /v1/admin/audience/fans/{fan_id}/tags/{tag}/remove`
- `GET|POST /v1/admin/audience/segments`
- `GET /v1/admin/audience/segments/{slug}/preview`
- `GET|POST /v1/admin/communications/campaigns`
- `POST /v1/admin/communications/campaigns/{campaign_id}/schedule`
- `POST /v1/admin/communications/campaigns/{campaign_id}/cancel`
- `GET /v1/admin/analytics/funnel`
- `GET /v1/admin/analytics/revenue`

Service-only delivery adapter:

- `GET /v1/internal/communications/campaigns/{campaign_id}/delivery-plan`
- `POST /v1/internal/communications/campaigns/{campaign_id}/complete`

## Campaign lifecycle

```text
draft
  -> scheduled --(communication.campaign_due at available_at)--> provider adapter
       |                                                   |
       +-> cancelled                                       +-> completed
```

Scheduling the same campaign again with the same timestamp is an idempotent replay. Completion with the same counts is also an idempotent replay.

The first delivery-plan request freezes the segment membership as `(campaign_id, fan_id)` rows so pagination is deterministic even if tags, city interest or other segment inputs change while a large campaign is being dispatched. It deliberately does **not** copy e-mail addresses or display names. Each page resolves the current fan record and rechecks active status plus the latest marketing consent, so unsubscribe/suppression remains authoritative until actual delivery.

Cancelling a scheduled campaign does not rewrite the established outbox state machine. If its already-created due event later reaches the adapter, the delivery-plan endpoint returns a conflict and the adapter must treat that as a terminal no-op rather than retrying it into the dead queue.

## Rollout

1. Deploy CrowdRelay with migration `0031_audience_intelligence.sql` while `communication_campaigns_enabled=false`.
2. Verify Fan 360, segment preview and analytics reads.
3. Update the existing n8n/provider adapter to ignore unknown event types and explicitly handle `communication.campaign_due` by calling the internal delivery-plan endpoint page-by-page. Deduplicate provider work by campaign + fan, and treat cancelled/conflict responses as terminal no-ops.
4. Call the completion endpoint only after all pages have been handled; same-count completion is replay-safe.
5. Regression-test the existing confirmation/session/event announcement mail flow.
6. Enable `communication_campaigns_enabled` through the existing ecosystem feature-flag API.
7. Schedule a small test segment and verify snapshot count, delivery count and completion counters.

Rollback is additive: disable `communication_campaigns_enabled` first. Existing fan, ticket, mail, webhook and n8n state does not depend on these tables.
