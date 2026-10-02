-- FAN SCOUT slice 3: one small, concrete thing an active Latarnik can do.
--
-- A mission is never "share us everywhere" or "become an ambassador". It is one
-- question about one real thing (a show in their city, a new release), one tap,
-- and a share text the person can send to one friend as it stands. It is offered
-- inside the Latarnik's own signed-in Signal session — nothing is sent to them —
-- and measured only by what it brings: a referred person who arrives through
-- the Latarnik's own referral code after the tap. A tap, a share and an invite
-- sent are never rewarded; only the existing referral rules decide rewards.
--
-- One open mission per role at a time (partial unique index), so a Latarnik is
-- never handed a pile.

-- Missions reference a role by (workspace, id); 0410 only made (workspace, person) unique.
ALTER TABLE latarnik_roles
    ADD CONSTRAINT latarnik_roles_workspace_id_key UNIQUE (workspace_id, id);

CREATE TABLE latarnik_missions (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    role_id uuid NOT NULL,
    kind text NOT NULL CHECK (kind IN ('show_one_person', 'release_one_person')),
    -- What the mission is about, so it is explainable; deleting the show removes
    -- the mission with it.
    event_id uuid,
    content_source_id uuid,
    -- The single question put to the person, and the text they may send on.
    -- Rendered at offer time from facts (title, city, date, their own link) and
    -- frozen, so what they were shown is what is on file.
    prompt text NOT NULL CHECK (char_length(prompt) BETWEEN 1 AND 300),
    share_text text NOT NULL CHECK (char_length(share_text) BETWEEN 1 AND 600),
    status text NOT NULL DEFAULT 'offered'
        CHECK (status IN ('offered', 'tapped', 'completed', 'expired', 'dismissed')),
    offered_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    tapped_at timestamptz,
    completed_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT latarnik_missions_role_fk
        FOREIGN KEY (workspace_id, role_id)
        REFERENCES latarnik_roles (workspace_id, id) ON DELETE CASCADE,
    CONSTRAINT latarnik_missions_event_fk
        FOREIGN KEY (workspace_id, event_id)
        REFERENCES events (workspace_id, id) ON DELETE CASCADE,
    -- A show mission is about a show: without one it has no question to ask.
    CONSTRAINT latarnik_missions_show_has_event
        CHECK (kind <> 'show_one_person' OR event_id IS NOT NULL),
    CONSTRAINT latarnik_missions_tapped_has_time
        CHECK (status NOT IN ('tapped', 'completed') OR tapped_at IS NOT NULL),
    CONSTRAINT latarnik_missions_completed_has_time
        CHECK (status <> 'completed' OR completed_at IS NOT NULL),
    CHECK (expires_at > offered_at)
);
-- At most one mission is open per Latarnik.
CREATE UNIQUE INDEX latarnik_missions_one_open
    ON latarnik_missions (workspace_id, role_id)
    WHERE status IN ('offered', 'tapped');
CREATE INDEX latarnik_missions_role_idx
    ON latarnik_missions (workspace_id, role_id, offered_at DESC);
