-- A headline for the failure the brief could not name.
--
-- The brief's headline set covers what the agent did, what it is waiting on and
-- what it cannot see. None of them fit the state production is actually in: the
-- discovery sweep runs, succeeds, reports nothing, and every outreach table
-- stays at zero while the action ledger reads green. Under the existing set
-- that reads as `worked`, which is the most misleading answer available.
--
-- `blind` is the closest existing headline and it is the wrong one. Blind means
-- the agent cannot measure a platform. This means the agent cannot find anybody
-- new to reach, which is the growth loop rather than the instrumentation, so it
-- outranks `blind` and sits below `failing` — a failed action is already
-- visible in the ledger and this is not.

ALTER TABLE viryaos_operator_briefs
    DROP CONSTRAINT viryaos_operator_briefs_headline_check;

ALTER TABLE viryaos_operator_briefs
    ADD CONSTRAINT viryaos_operator_briefs_headline_check CHECK (headline IN (
        'worked', 'blind', 'discovery_read_nothing', 'failing',
        'awaiting_approval', 'work_parked', 'disabled_with_work_waiting',
        'approval_stale'
    ));
