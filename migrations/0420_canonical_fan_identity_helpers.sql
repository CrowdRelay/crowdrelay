-- Exact canonical identity helpers for fan-owned durable history.
--
-- Some rows intentionally stay pinned to the historical fan id during merge
-- (for example referral codes/attributions). Readers and state machines need a
-- single deterministic way to resolve that historical row to the current live
-- person without timing or email heuristics.

CREATE OR REPLACE FUNCTION canonical_fan_id(
    p_workspace_id uuid,
    p_fan_id uuid
)
RETURNS uuid
LANGUAGE sql
STABLE
PARALLEL SAFE
AS $$
    WITH RECURSIVE lineage(id, merged_into_fan_id) AS (
        SELECT fan.id, fan.merged_into_fan_id
          FROM fans fan
         WHERE fan.workspace_id = p_workspace_id
           AND fan.id = p_fan_id
        UNION ALL
        SELECT parent.id, parent.merged_into_fan_id
          FROM lineage child
          JOIN fans parent
            ON parent.workspace_id = p_workspace_id
           AND parent.id = child.merged_into_fan_id
    )
    SELECT id
      FROM lineage
     WHERE merged_into_fan_id IS NULL
     LIMIT 1;
$$;

CREATE OR REPLACE FUNCTION canonical_fan_family(
    p_workspace_id uuid,
    p_fan_id uuid
)
RETURNS TABLE(fan_id uuid)
LANGUAGE sql
STABLE
PARALLEL SAFE
AS $$
    WITH RECURSIVE lineage(id, merged_into_fan_id) AS (
        SELECT fan.id, fan.merged_into_fan_id
          FROM fans fan
         WHERE fan.workspace_id = p_workspace_id
           AND fan.id = p_fan_id
        UNION ALL
        SELECT parent.id, parent.merged_into_fan_id
          FROM lineage child
          JOIN fans parent
            ON parent.workspace_id = p_workspace_id
           AND parent.id = child.merged_into_fan_id
    ), root(id) AS (
        SELECT id
          FROM lineage
         WHERE merged_into_fan_id IS NULL
         LIMIT 1
    ), family(member_id) AS (
        SELECT id FROM root
        UNION ALL
        SELECT child.id
          FROM family
          JOIN fans child
            ON child.workspace_id = p_workspace_id
           AND child.merged_into_fan_id = family.member_id
    )
    SELECT member_id FROM family;
$$;
