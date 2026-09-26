-- 'organiser' joins the outreach target vocabulary: a festival, contest or
-- cultural-centre booker who programmes events and is approached with a gig
-- request, not a release review. Until now the only pitchable kind they could
-- be filed under was 'press', which wrote them the wrong letter.
--
-- Eight lists move together: the relationship ledger's CHECK, the candidate
-- ledger's CHECK, the wave and wave-outcome tables' and the kind-learning
-- CHECKs (a wave of organiser pitches must be able to learn), the agent
-- staging table's CHECK (the gate `AGENT_TARGET_KINDS` in
-- `worker::agent_outcomes` mirrors — the vocabulary gate asserts parity), the
-- drive-contact staging CHECK so a sheet cell reading "organizator" can
-- carry the suggestion through promote, and the reply-classification CHECK
-- so an organiser's answer can still be triaged.

ALTER TABLE outreach_targets
    DROP CONSTRAINT IF EXISTS outreach_targets_target_kind_check;
ALTER TABLE outreach_targets
    ADD CONSTRAINT outreach_targets_target_kind_check CHECK (target_kind IN (
        'playlist','radio','press','creator','support_slot','endorsement','media_patronage',
        'organiser','agent','label'
    ));

ALTER TABLE agent_outreach_targets
    DROP CONSTRAINT IF EXISTS agent_outreach_targets_target_kind_check;
ALTER TABLE agent_outreach_targets
    ADD CONSTRAINT agent_outreach_targets_target_kind_check
    CHECK (target_kind IN (
        'press', 'radio', 'playlist', 'media_patronage',
        'endorsement', 'creator', 'community', 'organiser'
    ));

ALTER TABLE drive_contacts
    DROP CONSTRAINT IF EXISTS drive_contacts_suggested_kind_check;
ALTER TABLE drive_contacts
    ADD CONSTRAINT drive_contacts_suggested_kind_check
    CHECK (suggested_kind IS NULL OR suggested_kind IN (
        'fan', 'press', 'radio', 'playlist', 'media_patronage', 'endorsement',
        'creator', 'promoter', 'venue', 'festival', 'organiser',
        'agent', 'label', 'booking_agent', 'talent_buyer'
    ));

ALTER TABLE reply_classifications
    DROP CONSTRAINT IF EXISTS reply_classifications_target_kind_check;
ALTER TABLE reply_classifications
    ADD CONSTRAINT reply_classifications_target_kind_check
    CHECK (target_kind IN (
        'playlist', 'radio', 'press', 'creator', 'support_slot',
        'endorsement', 'media_patronage', 'organiser',
        'promoter', 'venue', 'festival', 'agent', 'label'
    ));

-- The pitchable-kind set, kept identical across the four tables that hold it.
ALTER TABLE outreach_candidates
    DROP CONSTRAINT IF EXISTS outreach_candidates_target_kind_check;
ALTER TABLE outreach_candidates
    ADD CONSTRAINT outreach_candidates_target_kind_check
    CHECK (target_kind IN (
        'playlist', 'radio', 'press', 'creator', 'support_slot',
        'endorsement', 'media_patronage', 'organiser'
    ));

ALTER TABLE outreach_waves
    DROP CONSTRAINT IF EXISTS outreach_waves_target_kind_check;
ALTER TABLE outreach_waves
    ADD CONSTRAINT outreach_waves_target_kind_check
    CHECK (target_kind IN (
        'playlist', 'radio', 'press', 'creator', 'support_slot',
        'endorsement', 'media_patronage', 'organiser'
    ));

ALTER TABLE outreach_wave_outcomes
    DROP CONSTRAINT IF EXISTS outreach_wave_outcomes_target_kind_check;
ALTER TABLE outreach_wave_outcomes
    ADD CONSTRAINT outreach_wave_outcomes_target_kind_check
    CHECK (target_kind IN (
        'playlist', 'radio', 'press', 'creator', 'support_slot',
        'endorsement', 'media_patronage', 'organiser'
    ));

ALTER TABLE outreach_kind_learning
    DROP CONSTRAINT IF EXISTS outreach_kind_learning_target_kind_check;
ALTER TABLE outreach_kind_learning
    ADD CONSTRAINT outreach_kind_learning_target_kind_check
    CHECK (target_kind IN (
        'playlist', 'radio', 'press', 'creator', 'support_slot',
        'endorsement', 'media_patronage', 'organiser'
    ));
