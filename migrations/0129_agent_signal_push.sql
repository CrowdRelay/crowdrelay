-- Sprint 5: allow agent-originated Signal push deliveries.
--
-- The signal-inviter agent drafts push notifications that, after operator
-- approval, are materialized as fan_push_deliveries rows by the autopilot
-- executor. These are distinct from campaign-driven pushes
-- (communication_campaign) and nearby-concert pushes (nearby_concert) so
-- they get their own source_kind for attribution and retention.

ALTER TABLE fan_push_deliveries
    DROP CONSTRAINT IF EXISTS fan_push_deliveries_source_kind_check,
    ADD CONSTRAINT fan_push_deliveries_source_kind_check CHECK (
        source_kind IN (
            'nearby_concert',
            'communication_campaign',
            'show_checklist',
            'beacon_nearby_concert',
            'agent_signal_push'
        )
    );
