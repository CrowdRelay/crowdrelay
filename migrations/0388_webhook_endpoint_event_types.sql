-- Per-endpoint event subscription for webhook fanout.
--
-- `materialize_deliveries_batch` fans every outbox event out to every active
-- endpoint. The n8n bridge answers 422 for any event type its route map does
-- not know, which the transport records as a permanent cancellation — so
-- every unrouted event produced a dead delivery row: press pitches,
-- band-facing reports and discovery requests have been dying there since
-- their emit sites landed (2026-09-13 and 2026-09-26 incidents).
--
-- `event_types` lists the event types an endpoint accepts; NULL keeps the
-- previous all-events behaviour, so tenant webhooks that want the full fire
-- hose are unaffected. The n8n bridge endpoint carries the bridge route
-- map's keys — an event with no route produces no delivery instead of a
-- permanently cancelled one.
ALTER TABLE webhook_endpoints
    ADD COLUMN event_types text[];
