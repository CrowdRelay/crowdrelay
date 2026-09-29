-- A curator DM sent by hand from the curator queue is an outreach
-- interaction, but a handle is never an outreach target: targets are
-- keyed on contact_email and a handle has none. The interaction gains a
-- nullable candidate_id instead, with target_id and candidate_id mutually
-- exclusive so a row still names exactly one party.
ALTER TABLE outreach_interactions
    ALTER COLUMN target_id DROP NOT NULL,
    ADD COLUMN candidate_id uuid,
    ADD CONSTRAINT outreach_interactions_one_party_check
        CHECK ((target_id IS NOT NULL) <> (candidate_id IS NOT NULL)),
    ADD CONSTRAINT outreach_interactions_candidate_fk
        FOREIGN KEY (workspace_id, candidate_id)
        REFERENCES outreach_candidates (workspace_id, id) ON DELETE CASCADE;

-- The existing (workspace_id, target_id, source_key) unique constraint
-- cannot dedupe candidate rows (target_id is NULL there), so candidate
-- sends get their own partial unique index — one source_key per candidate.
CREATE UNIQUE INDEX outreach_interactions_candidate_source_key
    ON outreach_interactions (workspace_id, candidate_id, source_key)
    WHERE candidate_id IS NOT NULL;
