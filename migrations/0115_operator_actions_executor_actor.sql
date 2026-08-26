-- Allow executor ingestions to be recorded under their own actor type.
--
-- Every operator action was recorded as `admin_api_key` because the CHECK
-- allowed nothing else, so an executor posting discovery candidates or
-- delivery faults was indistinguishable from an administrator in the ledger.
-- The action name identified the source unambiguously today, but the ledger
-- could not express who did it — and a later read model that groups by actor
-- would merge every executor ingestion into the admin surface.
--
-- `executor` covers any internal/commerce-key caller: the n8n executor
-- posting candidates, faults and replies, and the event sync writing tracker
-- observations. Admin API keys stay `admin_api_key`; no existing row moves.

ALTER TABLE operator_actions
    DROP CONSTRAINT IF EXISTS operator_actions_actor_type_check;
ALTER TABLE operator_actions
    ADD CONSTRAINT operator_actions_actor_type_check
        CHECK (actor_type IN ('admin_api_key', 'executor'));
