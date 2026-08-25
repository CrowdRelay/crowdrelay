-- New brief headline: the execution plane itself is dead.
-- Parked actions exist, no executor has a fresh heartbeat. Different fix
-- from a capability gap (deploy/fix n8n) and different urgency from
-- work_parked (which covers partial gaps).
ALTER TABLE viryaos_operator_briefs DROP CONSTRAINT viryaos_operator_briefs_headline_check;
ALTER TABLE viryaos_operator_briefs ADD CONSTRAINT viryaos_operator_briefs_headline_check
    CHECK (headline IN (
        'worked', 'blind', 'discovery_read_nothing', 'failing',
        'awaiting_approval', 'work_parked', 'disabled_with_work_waiting',
        'approval_stale', 'execution_plane_dead'
    ));
