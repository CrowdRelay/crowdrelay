-- FAN SCOUT slice 1C: what the band actually said to a prospect, and what came
-- of it.
--
-- Until now the prospect layer recorded what a person did and a queue said what
-- to do about it, but nothing recorded that the band had done it. The queue
-- therefore went on recommending "answer this person" after the reply lane had
-- already answered them, and a conversion could not be attributed to anything.
--
-- A touch is one thing the band said to one prospect, in the thread the person
-- spoke in. It is written by observing the surface that sent it (today the
-- owned-reply lane), never by the evaluator: the evaluator decides, an executor
-- acts, and this is the executor's receipt.

CREATE TABLE fan_prospect_touches (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    prospect_id uuid NOT NULL,
    -- engage: answered in context. invite: carried the join link.
    kind text NOT NULL CHECK (kind IN ('engage', 'invite')),
    -- The surface that sent it and its own row, so reading the same send twice
    -- cannot record two touches.
    source text NOT NULL CHECK (btrim(source) <> ''),
    source_ref text NOT NULL CHECK (btrim(source_ref) <> ''),
    -- The tracked link the touch carried, when it carried one. The only road
    -- from a touch to a conversion: click -> visitor -> arrival.
    smart_link_id uuid,
    -- When the band spoke, as the sending surface recorded it.
    touched_at timestamptz NOT NULL,
    -- Joins the touch to the person's conversion for the one-trace proof.
    trace_id uuid NOT NULL DEFAULT gen_random_uuid(),
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT fan_prospect_touches_prospect_fk
        FOREIGN KEY (workspace_id, prospect_id)
        REFERENCES fan_prospects (workspace_id, id) ON DELETE CASCADE,
    CONSTRAINT fan_prospect_touches_link_fk
        FOREIGN KEY (smart_link_id) REFERENCES smart_links (id) ON DELETE SET NULL,
    CONSTRAINT fan_prospect_touches_invite_has_link
        CHECK (kind <> 'invite' OR smart_link_id IS NOT NULL),
    UNIQUE (workspace_id, source, source_ref)
);
CREATE INDEX fan_prospect_touches_prospect_idx
    ON fan_prospect_touches (workspace_id, prospect_id, touched_at DESC);
CREATE INDEX fan_prospect_touches_link_idx
    ON fan_prospect_touches (smart_link_id)
    WHERE smart_link_id IS NOT NULL;
