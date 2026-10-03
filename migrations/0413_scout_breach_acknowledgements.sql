-- A scout-lane halt is cleared by a person, not by the calendar and not by the
-- machine.
--
-- `scout_lane::breaches` (0408's touches, #495) halts every reply sender when the
-- lane's envelope looks breached. Without a way to say "I have looked at this", a
-- breach is a halt for its whole look-back window with no recourse: on 2026-10-03,
-- hours after the first deploy, two test replies to the owner's own account four
-- minutes apart halted every reply sender (Instagram, Facebook, Reddit, YouTube)
-- for a week.
--
-- An acknowledgement is a person's recorded decision that the breaches of one kind
-- seen up to that moment have been reviewed. It never loosens the rule: only touches
-- made *after* the newest acknowledgement of a kind can breach it again, so the same
-- fault recurring halts the lane again at once.
CREATE TABLE scout_breach_acknowledgements (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    breach text NOT NULL CHECK (breach IN (
        'contacted_suppressed', 'over_rate', 'invite_without_route', 'untracked_link')),
    acknowledged_by text NOT NULL CHECK (char_length(acknowledged_by) BETWEEN 1 AND 120),
    -- Why it is safe to resume: required, so an acknowledgement is a reason, not a click.
    note text NOT NULL CHECK (btrim(note) <> '' AND char_length(note) <= 1000),
    acknowledged_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX scout_breach_acknowledgements_latest_idx
    ON scout_breach_acknowledgements (workspace_id, breach, acknowledged_at DESC);
