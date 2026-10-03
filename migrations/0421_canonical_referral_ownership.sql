-- Canonical referral ownership: one real referred person may credit at most one
-- canonical referrer. Identity merges can reveal that two previously-distinct
-- referred rows were the same human and carried different referral codes.
-- Choosing a winner by timestamp would manufacture causality, so ambiguous
-- ownership is NULL and contributes to no growth/reward count until one side
-- is explicitly reversed.
--
-- Historical attribution rows remain exact and immutable-in-meaning; these
-- helpers are read semantics over current canonical identity.

CREATE OR REPLACE FUNCTION canonical_live_referral_owner_id(
    p_workspace_id uuid,
    p_referred_fan_id uuid
)
RETURNS uuid
LANGUAGE sql
STABLE
PARALLEL SAFE
AS $$
    WITH referred_root AS (
        SELECT canonical_fan_id(p_workspace_id, p_referred_fan_id) AS id
    ), owners AS (
        SELECT DISTINCT canonical_fan_id(
            p_workspace_id, attribution.referrer_fan_id
        ) AS referrer_id
        FROM referral_attributions attribution
        CROSS JOIN referred_root
        WHERE attribution.workspace_id = p_workspace_id
          AND attribution.referred_fan_id IN (
              SELECT fan_id FROM canonical_fan_family(
                  p_workspace_id, referred_root.id
              )
          )
          AND attribution.status IN ('pending','qualified')
          AND canonical_fan_id(
              p_workspace_id, attribution.referrer_fan_id
          ) IS DISTINCT FROM referred_root.id
    )
    SELECT referrer_id
    FROM owners
    WHERE (SELECT count(*) FROM owners) = 1
    LIMIT 1;
$$;

CREATE OR REPLACE FUNCTION canonical_qualified_referral_owner_id(
    p_workspace_id uuid,
    p_referred_fan_id uuid
)
RETURNS uuid
LANGUAGE sql
STABLE
PARALLEL SAFE
AS $$
    WITH referred_root AS (
        SELECT canonical_fan_id(p_workspace_id, p_referred_fan_id) AS id
    ), owners AS (
        SELECT DISTINCT canonical_fan_id(
            p_workspace_id, attribution.referrer_fan_id
        ) AS referrer_id
        FROM referral_attributions attribution
        CROSS JOIN referred_root
        WHERE attribution.workspace_id = p_workspace_id
          AND attribution.referred_fan_id IN (
              SELECT fan_id FROM canonical_fan_family(
                  p_workspace_id, referred_root.id
              )
          )
          AND attribution.status = 'qualified'
          AND canonical_fan_id(
              p_workspace_id, attribution.referrer_fan_id
          ) IS DISTINCT FROM referred_root.id
    )
    SELECT referrer_id
    FROM owners
    WHERE (SELECT count(*) FROM owners) = 1
    LIMIT 1;
$$;

CREATE OR REPLACE FUNCTION canonical_qualified_referral_count(
    p_workspace_id uuid,
    p_referrer_fan_id uuid,
    p_qualified_before timestamptz
)
RETURNS bigint
LANGUAGE sql
STABLE
PARALLEL SAFE
AS $$
    WITH referrer AS (
        SELECT canonical_fan_id(p_workspace_id, p_referrer_fan_id) AS id
    ), referred AS (
        SELECT DISTINCT canonical_fan_id(
            p_workspace_id, attribution.referred_fan_id
        ) AS id
        FROM referral_attributions attribution
        CROSS JOIN referrer
        WHERE attribution.workspace_id = p_workspace_id
          AND attribution.referrer_fan_id IN (
              SELECT fan_id FROM canonical_fan_family(
                  p_workspace_id, referrer.id
              )
          )
          AND attribution.status = 'qualified'
          AND (
              p_qualified_before IS NULL
              OR attribution.qualified_at <= p_qualified_before
          )
          AND canonical_fan_id(
              p_workspace_id, attribution.referred_fan_id
          ) IS DISTINCT FROM referrer.id
    )
    SELECT count(*)::bigint
    FROM referred
    CROSS JOIN referrer
    WHERE canonical_qualified_referral_owner_id(
        p_workspace_id, referred.id
    ) = referrer.id;
$$;
