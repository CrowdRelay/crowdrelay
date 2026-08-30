-- Evidence projection rebuild: growth_episodes from growth_evidence.
--
-- This function proves that viryaos_growth_episodes is a rebuildable
-- projection of the source-of-truth viryaos_growth_evidence table.
--
-- Ownership:
--   viryaos_growth_evidence  = source of truth (immutable observed facts)
--   viryaos_growth_episodes  = rebuildable projection (derived aggregate)
--
-- The function truncates and rebuilds growth_episodes from growth_evidence.
-- It is idempotent: rebuild(rebuild(E)) == rebuild(E).
--
-- Usage:
--   SELECT viryaos_rebuild_growth_episodes_from_evidence();           -- all
--   SELECT viryaos_rebuild_growth_episodes_from_evidence($workspace); -- one

CREATE OR REPLACE FUNCTION viryaos_rebuild_growth_episodes_from_evidence(
    p_workspace_id uuid DEFAULT NULL
)
RETURNS integer
LANGUAGE plpgsql
AS $$
DECLARE
    inserted_count integer;
BEGIN
    -- Truncate existing episodes for the workspace (or all if NULL).
    IF p_workspace_id IS NOT NULL THEN
        DELETE FROM viryaos_growth_episodes WHERE workspace_id = p_workspace_id;
    ELSE
        TRUNCATE TABLE viryaos_growth_episodes;
    END IF;

    -- Rebuild from the source-of-truth evidence table.
    INSERT INTO viryaos_growth_episodes (
        workspace_id, action_id, opportunity_id, episode_id,
        channel, estimated_reach, treatment, propensity,
        predicted_fans, predicted_signal_installs, context,
        observed_fans, observed_incremental_fans, durable_fans_30d,
        actual_reach, converted, resolved_at, updated_at
    )
    SELECT
        ge.workspace_id,
        ge.action_id,
        ge.opportunity_id,
        ge.episode_id,
        ge.channel,
        ge.estimated_reach,
        ge.treatment,
        ge.propensity,
        ge.predicted_fans,
        ge.predicted_signal_installs,
        ge.context,
        ge.observed_fans,
        ge.observed_incremental_fans,
        ge.durable_fans_30d,
        ge.actual_reach,
        ge.converted,
        ge.resolved_at,
        now()
    FROM viryaos_growth_evidence ge
    WHERE p_workspace_id IS NULL OR ge.workspace_id = p_workspace_id;

    GET DIAGNOSTICS inserted_count = ROW_COUNT;

    RETURN inserted_count;
END;
$$;
