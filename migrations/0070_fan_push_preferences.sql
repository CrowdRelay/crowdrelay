-- Additive fan notification preferences. Missing rows preserve historical
-- behaviour: all fan categories enabled and quiet hours disabled.

CREATE TABLE fan_push_preferences (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    fan_id uuid NOT NULL,
    shows_enabled boolean NOT NULL DEFAULT true,
    releases_enabled boolean NOT NULL DEFAULT true,
    community_enabled boolean NOT NULL DEFAULT true,
    merch_enabled boolean NOT NULL DEFAULT true,
    quiet_hours_enabled boolean NOT NULL DEFAULT false,
    quiet_start_minute smallint NOT NULL DEFAULT 1320 CHECK (quiet_start_minute BETWEEN 0 AND 1439),
    quiet_end_minute smallint NOT NULL DEFAULT 480 CHECK (quiet_end_minute BETWEEN 0 AND 1439),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, fan_id),
    CONSTRAINT fan_push_preferences_fan_fk
        FOREIGN KEY (workspace_id, fan_id)
        REFERENCES fans (workspace_id, id)
        ON DELETE CASCADE
);

ALTER TABLE fan_push_deliveries
    ADD COLUMN category text NOT NULL DEFAULT 'essential';
ALTER TABLE fan_push_deliveries
    ADD CONSTRAINT fan_push_deliveries_category_check
    CHECK (category IN ('essential','shows','releases','community','merch','staff'));

-- Category is part of delivery policy, not presentation metadata. Historical
-- staff deliveries predate this column and are migrated explicitly before the
-- invariant is installed; a fan/beacon delivery can never masquerade as staff.
UPDATE fan_push_deliveries
SET category = 'staff'
WHERE audience_kind = 'staff';

ALTER TABLE fan_push_deliveries
    ADD CONSTRAINT fan_push_deliveries_staff_category_check
    CHECK (
        (audience_kind = 'staff' AND category = 'staff')
        OR (audience_kind <> 'staff' AND category <> 'staff')
    );
