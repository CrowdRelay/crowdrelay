-- Add 'event_announcement' as a valid source_kind for fan_push_deliveries
-- so event announcements can be delivered as push notifications to fans
-- with active push endpoints and marketing consent, in addition to the
-- existing webhook outbox path.
--
-- The event_sync announcement pipeline creates these rows in the same
-- transaction as the outbox events, so a fan with a push endpoint gets
-- both channels (webhook for email, push for mobile) from one announcement.
--
-- Idempotent: the constraint drop+add is safe to re-run.

ALTER TABLE fan_push_deliveries
    DROP CONSTRAINT IF EXISTS fan_push_deliveries_source_kind_check;

ALTER TABLE fan_push_deliveries
    ADD CONSTRAINT fan_push_deliveries_source_kind_check CHECK (
        source_kind IN (
            'nearby_concert',
            'communication_campaign',
            'show_checklist',
            'beacon_nearby_concert',
            'agent_signal_push',
            'event_announcement'
        )
    );
