-- The thread follow-up lane: a 'thread' subject for the opportunities the
-- act's own handwritten, imported threads open, and a 'threads' anchor kind
-- so those follow-ups batch into a monthly wave per kind — approved as one
-- read, never as loose cards. See `WaveAnchor::Threads` and the
-- `thread_followup` supply branch in `autopilot::outreach_supply`.
ALTER TABLE outreach_opportunities DROP CONSTRAINT outreach_opportunities_subject_kind_check;
ALTER TABLE outreach_opportunities ADD CONSTRAINT outreach_opportunities_subject_kind_check
    CHECK (subject_kind IN ('release', 'event', 'catalogue', 'band', 'thread'));
ALTER TABLE outreach_waves DROP CONSTRAINT outreach_waves_anchor_kind_check;
ALTER TABLE outreach_waves ADD CONSTRAINT outreach_waves_anchor_kind_check
    CHECK (anchor_kind IN ('release', 'event', 'catalogue', 'threads'));
