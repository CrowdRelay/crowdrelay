-- Foreign keys onto fans/events that nothing indexed on the referencing side.
-- Deleting or merging a fan (erasure, identity merge) has to find child rows by
-- these columns; without an index each such delete scans the child table.
-- fk-index-ratchet.py names exactly these four.
CREATE INDEX IF NOT EXISTS fan_capture_contexts_fan_idx
    ON fan_capture_contexts (fan_id);
CREATE INDEX IF NOT EXISTS fan_prospects_linked_fan_idx
    ON fan_prospects (workspace_id, linked_fan_id);
CREATE INDEX IF NOT EXISTS latarnik_missions_event_idx
    ON latarnik_missions (workspace_id, event_id);
CREATE INDEX IF NOT EXISTS organic_fan_exclusions_fan_idx
    ON organic_fan_exclusions (fan_id);
