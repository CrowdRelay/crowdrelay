-- Whether we are actually in a community, as distinct from whether we watch it.
--
-- `discovery_places.status` is active/archived/blocked and describes whether
-- the place is worth observing. It says nothing about our relationship with
-- it, so the console could list 66 tracked communities and an operator had no
-- way to record "I joined this one", "this one rejected us", or "this is not
-- a fit". Every visit to the page started from zero, which is why joining
-- never became a task anybody picked up.
--
-- Membership is a fact about the place, so it lives on the place rather than
-- in a side table nothing else joins to.
--
-- `not_a_fit` is deliberately separate from `rejected`: one is our judgement
-- and one is theirs, and collapsing them loses the reason a place should not
-- be revisited.
ALTER TABLE discovery_places
    ADD COLUMN IF NOT EXISTS membership_state text NOT NULL DEFAULT 'not_joined'
        CHECK (membership_state IN ('not_joined', 'joining', 'joined', 'rejected', 'not_a_fit')),
    ADD COLUMN IF NOT EXISTS membership_note text
        CHECK (membership_note IS NULL OR char_length(membership_note) <= 1000),
    ADD COLUMN IF NOT EXISTS membership_changed_at timestamptz,
    ADD COLUMN IF NOT EXISTS membership_changed_by text
        CHECK (membership_changed_by IS NULL OR char_length(membership_changed_by) <= 120);

COMMENT ON COLUMN discovery_places.membership_state IS
    'Our relationship with the community: not_joined, joining, joined, rejected (they said no), not_a_fit (we decided no).';
COMMENT ON COLUMN discovery_places.membership_note IS
    'Why the state is what it is — the rule that got us rejected, the reason it is not a fit, the channel we post in.';

-- The console sorts the work queue by "not joined, biggest first", so the
-- index matches that read rather than the primary key.
CREATE INDEX IF NOT EXISTS discovery_places_membership_idx
    ON discovery_places (workspace_id, membership_state, member_count DESC NULLS LAST)
    WHERE status = 'active';
