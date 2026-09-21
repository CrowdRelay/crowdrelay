-- Standing approvals: the operator's "yes, for this one, stop asking me".
--
-- The authority ladder answers how much a context may do and how far a class
-- of action may reach. Neither can express the decision an operator actually
-- makes about a community they have already read three drafts from, which is
-- "this one is fine, go". Without somewhere to record that, the only way to
-- say it is to approve each post, and the queue expires at 72 hours faster
-- than a person empties it. Measured in production: seven drafts, four
-- approvals, four hundred and twelve opportunities, zero posts published.
--
-- `action_class.rs` has promised this mechanism since it was written --
-- "widening the agent's autonomy later is a row update and a set of
-- pre-approved templates" -- and the row update shipped while the
-- pre-approved templates did not. This is that half.
--
-- `auto_post_platforms` already carries the same idea for whole channels and
-- states the reason plainly: a flag saying "publish to Telegram without
-- asking me" is an approval, and asking again per post is asking twice. That
-- works because a channel is something an operator can judge once. A
-- community is the same kind of object. A single post is not.
--
-- # What a standing approval is and is not
--
-- It satisfies the *approval* requirement for one named target, and nothing
-- else. Specifically it cannot:
--
--   * act where the operator said not to act. A context at `observe` or
--     `recommend` produces no action to approve in the first place, and a
--     standing row does not manufacture one. Only `require_approval` is
--     answered here.
--   * cover money. `paid` is refused by CHECK, not by a code path that could
--     be reordered. No posture has ever let spend run unattended and no row
--     in this table may be the first thing that does.
--   * cover a target nobody named. There is no wildcard: the key is one
--     concrete target, so "approve everything" is not expressible.
--
-- # Why expiry is not nullable
--
-- A standing approval with no end is a decision nobody revisits. The band's
-- relationship with a community changes, moderators change, and the reason
-- the operator said yes decays without announcing itself. An expiry makes
-- the grant come back for a second look while the first look is still
-- remembered. Ninety days is long enough that it is not ceremony and short
-- enough that a season's worth of drift cannot hide behind it.

CREATE TABLE viryaos_standing_approvals (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    -- The action kind this grant answers for, e.g. 'community.engage.request'.
    -- Deliberately the kind rather than the context: a context can hold
    -- several kinds and an operator who approved a forum post has not
    -- approved a press pitch that happens to share its context.
    action_kind text NOT NULL CHECK (btrim(action_kind) <> '' AND char_length(action_kind) <= 100),
    -- The one target this grant covers. A community target id for a community
    -- post. Opaque text because the target's own identity type differs per
    -- kind, and a text key cannot accidentally join to the wrong table.
    target_key text NOT NULL CHECK (btrim(target_key) <> '' AND char_length(target_key) <= 200),
    -- Recorded so the reader of this row knows what was being trusted without
    -- re-deriving it from the action kind, and so the CHECK below can refuse
    -- money without consulting anything else.
    action_class text NOT NULL CHECK (action_class IN (
        'first_party_reversible', 'owned_audience', 'third_party'
    )),
    granted_by text NOT NULL CHECK (btrim(granted_by) <> '' AND char_length(granted_by) <= 200),
    granted_at timestamptz NOT NULL DEFAULT now(),
    -- Not nullable. See the header: a grant with no end is a grant nobody
    -- revisits.
    expires_at timestamptz NOT NULL CHECK (expires_at > granted_at),
    revoked_at timestamptz,
    revoked_by text CHECK (revoked_by IS NULL OR btrim(revoked_by) <> ''),
    -- Why the operator said yes, in their words, for whoever reads the row
    -- when it comes back up for renewal.
    note text CHECK (note IS NULL OR char_length(note) <= 240),
    PRIMARY KEY (workspace_id, action_kind, target_key),
    -- A revocation carries who did it, the same way the grant does.
    CONSTRAINT viryaos_standing_approvals_revocation_is_attributed
        CHECK ((revoked_at IS NULL) = (revoked_by IS NULL))
);

COMMENT ON TABLE viryaos_standing_approvals IS
    'One operator grant per (action kind, target): this target may act without '
    'a per-action approval until it expires or is revoked. Answers '
    'require_approval only; never observe, recommend or paid.';

-- The worker asks one question of this table -- "is there a live grant for
-- this kind and target" -- once per action it is about to write.
CREATE INDEX viryaos_standing_approvals_live_idx
    ON viryaos_standing_approvals (workspace_id, action_kind, target_key)
    WHERE revoked_at IS NULL;
