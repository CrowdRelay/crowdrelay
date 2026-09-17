-- Sprint 2 — two durable pieces:
--
-- `viryaos_draft_revisions` is the record of an operator fixing a nearly-right
-- draft instead of rejecting it (2.9/2.10, §4d-3.1/§4d-3.2). One row per field
-- the operator actually changed on approve: the machine's words, the band's
-- words, and how far they moved. The distance is the only honest measure of
-- whether voice-matching works — it should fall over time, and §4c's n-rules
-- apply before anyone reads a trend into it.
--
-- `viryaos_growth_envelope.daily_third_party_touches` adds the daily ceiling
-- that sits atop the per-subject cooldown (2.4). The weekly budget bounds a
-- week; the daily bound stops a bad morning — venues, curators and press are
-- finite relationships and a broken segment that mails everyone at 09:00
-- cannot be unmailed at 09:05.

CREATE TABLE viryaos_draft_revisions (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    action_id uuid NOT NULL,
    operation_id uuid NOT NULL,
    field text NOT NULL CHECK (btrim(field) <> '' AND char_length(field) <= 64),
    before_text text NOT NULL,
    after_text text NOT NULL,
    distance_chars integer NOT NULL CHECK (distance_chars >= 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    -- One revision per field per action: an action can be approved once, and a
    -- second row for the same field would be a second approve that the status
    -- transition already refused.
    CONSTRAINT viryaos_draft_revisions_action_field
        UNIQUE (workspace_id, action_id, field),
    CONSTRAINT viryaos_draft_revisions_action_fk
        FOREIGN KEY (workspace_id, action_id)
        REFERENCES viryaos_autopilot_actions (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT viryaos_draft_revisions_operation_fk
        FOREIGN KEY (operation_id)
        REFERENCES operator_actions (id)
        ON DELETE RESTRICT
);

CREATE INDEX viryaos_draft_revisions_created_idx
    ON viryaos_draft_revisions (workspace_id, created_at DESC);

ALTER TABLE viryaos_growth_envelope
    ADD COLUMN daily_third_party_touches integer NOT NULL DEFAULT 3
        CHECK (daily_third_party_touches BETWEEN 0 AND 100);
