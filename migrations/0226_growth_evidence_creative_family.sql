-- Which angle a post took, recorded against its outcome.
--
-- The brain decided whether to post and where; what the post said was chosen
-- by the drafting worker and never written down. Two posts to the same
-- community with the same predicted value could be a personal story and a
-- technical breakdown, and afterwards nothing could tell them apart — so the
-- variance creative causes reached the causal model as noise.
--
-- Nothing reads this column for a decision yet, and that is deliberate: there
-- is not one measured community post to learn from, and an estimator built
-- before its data is an estimator nobody has evaluated. The column exists now
-- because a label cannot be attached to an outcome after the fact, and the
-- first posts are about to happen.
--
-- The vocabulary is community-post specific. Press pitches and fan messages
-- have their own angles and will get their own values; pooling them here
-- would compare families that are not comparable.

ALTER TABLE viryaos_growth_evidence
    ADD COLUMN IF NOT EXISTS creative_family text
        CHECK (creative_family IS NULL OR creative_family IN (
            'story', 'riff', 'technical', 'identity', 'event'
        ));

COMMENT ON COLUMN viryaos_growth_evidence.creative_family IS
    'The angle the post was asked to take (story, riff, technical, identity, event). NULL for dispatches with no creative surface and for rows predating the column.';

CREATE INDEX IF NOT EXISTS viryaos_growth_evidence_creative_family_idx
    ON viryaos_growth_evidence (workspace_id, creative_family, timestamp DESC)
    WHERE creative_family IS NOT NULL;
