-- The roster's monthly share of one person's attention needs a ledger the
-- governor cannot carry (§4d-3).
--
-- `viryaos_contact_governor` is one row per (workspace, contact), updated in
-- place: it can say "not before Friday" but it cannot count. A budget asks a
-- different question — of the four things this roster wanted to tell this
-- person this month, how many already went out — and that is a count of
-- touches, not a window. `viryaos_contact_touches` is the append-only half:
-- one row per action that reserved a contact window.
--
-- Keyed (workspace_id, normalized_contact, action_id) because one action can
-- name several contacts — a booking letter carries its anchor plus its
-- recipients — while one action replayed must still count once, which the
-- primary key makes free. `action_id` foreign-keys the composite way the
-- governor's `last_action_id` does, and RESTRICTs for the same reason:
-- deleting an action must not erase the record that a person was contacted.
CREATE TABLE viryaos_contact_touches (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    normalized_contact text NOT NULL CHECK (
        btrim(normalized_contact) <> '' AND char_length(normalized_contact) <= 320
    ),
    action_id uuid NOT NULL,
    touched_at timestamptz NOT NULL,
    PRIMARY KEY (workspace_id, normalized_contact, action_id),
    CONSTRAINT viryaos_contact_touches_action_fk
        FOREIGN KEY (workspace_id, action_id)
        REFERENCES viryaos_autopilot_actions (workspace_id, id)
        ON DELETE RESTRICT
);

-- The budget read is a trailing-30-day count for one contact across the
-- organization; it filters on the contact first and this index is what keeps
-- that read off a scan of every workspace's history.
CREATE INDEX viryaos_contact_touches_contact_idx
    ON viryaos_contact_touches (normalized_contact, touched_at);
