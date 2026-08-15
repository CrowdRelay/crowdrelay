-- Staff/owner pre-gig checklist push support.
--
-- The existing fan push transport is intentionally generalized instead of
-- creating a second provider/ACK pipeline. Audience context is explicit so the
-- same physical Signal installation can hold independent fan and staff
-- subscriptions without one overwriting the other.

ALTER TABLE show_checklist_items
    ADD COLUMN IF NOT EXISTS section text NOT NULL DEFAULT 'logistics',
    ADD COLUMN IF NOT EXISTS sort_order integer NOT NULL DEFAULT 100;

ALTER TABLE show_checklist_items
    DROP CONSTRAINT IF EXISTS show_checklist_items_section_check;
ALTER TABLE show_checklist_items
    ADD CONSTRAINT show_checklist_items_section_check CHECK (
        section IN ('show_files','gear','media','logistics','gate','post_show')
    );
ALTER TABLE show_checklist_items
    DROP CONSTRAINT IF EXISTS show_checklist_items_sort_order_check;
ALTER TABLE show_checklist_items
    ADD CONSTRAINT show_checklist_items_sort_order_check CHECK (sort_order BETWEEN 1 AND 999);

-- Reclassify legacy items so old events keep useful history while new clients
-- can render the checklist in a predictable order.
UPDATE show_checklist_items SET section = 'logistics', sort_order = 10 WHERE item_key = 'announcement_published';
UPDATE show_checklist_items SET section = 'logistics', sort_order = 20 WHERE item_key = 'ticketing_verified';
UPDATE show_checklist_items SET section = 'logistics', sort_order = 30 WHERE item_key = 'staff_assigned';
UPDATE show_checklist_items SET section = 'gate', sort_order = 210 WHERE item_key = 'offline_snapshot_ready';
UPDATE show_checklist_items SET section = 'gate', sort_order = 220 WHERE item_key = 'gate_device_charged';
UPDATE show_checklist_items SET section = 'gate', sort_order = 230 WHERE item_key = 'backup_device_ready';
UPDATE show_checklist_items SET section = 'gate', sort_order = 240 WHERE item_key = 'network_tested';
UPDATE show_checklist_items SET section = 'logistics', sort_order = 80 WHERE item_key = 'guestlist_checked';
UPDATE show_checklist_items SET section = 'post_show', sort_order = 310 WHERE item_key = 'post_show_reconciliation';
UPDATE show_checklist_items SET section = 'post_show', sort_order = 320 WHERE item_key = 'post_show_report';

ALTER TABLE show_notification_emissions
    DROP CONSTRAINT IF EXISTS show_notification_emissions_phase_check;
ALTER TABLE show_notification_emissions
    ADD CONSTRAINT show_notification_emissions_phase_check
    CHECK (phase IN ('week', 'two_days', 'day', 'gate', 'followup'));

-- Generalize endpoint ownership. Existing rows remain fan rows by default.
ALTER TABLE fan_push_endpoints
    ADD COLUMN IF NOT EXISTS audience_kind text NOT NULL DEFAULT 'fan',
    ADD COLUMN IF NOT EXISTS principal_hash bytea;

ALTER TABLE fan_push_endpoints
    ALTER COLUMN fan_id DROP NOT NULL;

ALTER TABLE fan_push_endpoints
    DROP CONSTRAINT IF EXISTS fan_push_endpoints_workspace_id_installation_id_transport_key;
ALTER TABLE fan_push_endpoints
    DROP CONSTRAINT IF EXISTS fan_push_endpoints_audience_check;
ALTER TABLE fan_push_endpoints
    ADD CONSTRAINT fan_push_endpoints_audience_check CHECK (
        (audience_kind = 'fan' AND fan_id IS NOT NULL AND principal_hash IS NULL)
        OR
        (audience_kind = 'staff' AND fan_id IS NULL AND principal_hash IS NOT NULL
            AND octet_length(principal_hash) = 32)
    );
ALTER TABLE fan_push_endpoints
    ADD CONSTRAINT fan_push_endpoints_installation_audience_key
    UNIQUE (workspace_id, installation_id, transport, audience_kind);

CREATE INDEX fan_push_endpoints_active_staff_idx
    ON fan_push_endpoints (workspace_id, principal_hash, transport, id)
    WHERE audience_kind = 'staff' AND active AND invalidated_at IS NULL;

ALTER TABLE fan_push_deliveries
    ADD COLUMN IF NOT EXISTS audience_kind text NOT NULL DEFAULT 'fan';
ALTER TABLE fan_push_deliveries
    ALTER COLUMN fan_id DROP NOT NULL;
ALTER TABLE fan_push_deliveries
    DROP CONSTRAINT IF EXISTS fan_push_deliveries_source_kind_check;
ALTER TABLE fan_push_deliveries
    ADD CONSTRAINT fan_push_deliveries_source_kind_check
    CHECK (source_kind IN ('nearby_concert', 'communication_campaign', 'show_checklist'));
ALTER TABLE fan_push_deliveries
    DROP CONSTRAINT IF EXISTS fan_push_deliveries_audience_check;
ALTER TABLE fan_push_deliveries
    ADD CONSTRAINT fan_push_deliveries_audience_check CHECK (
        (audience_kind = 'fan' AND fan_id IS NOT NULL)
        OR
        (audience_kind = 'staff' AND fan_id IS NULL)
    );

CREATE INDEX fan_push_deliveries_staff_recent_idx
    ON fan_push_deliveries (workspace_id, created_at DESC, id DESC)
    WHERE audience_kind = 'staff';
