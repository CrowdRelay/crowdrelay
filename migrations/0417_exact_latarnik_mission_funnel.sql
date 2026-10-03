-- One canonical causal read for Latarnik mission performance.
--
-- A mission outcome is never inferred from "same referrer + nearby time".
-- The exact chain is:
--
--   mission.action_id / mission.smart_link_id
--     -> human click_events.anonymous_visitor_id
--     -> fan_acquisition_events for that same visitor + referrer
--     -> referral_attributions for that exact referred fan/code
--
-- Rows exist for human mission clicks even before a signup; referral fields are
-- NULL until that same visitor becomes a referred fan. This lets click cadence,
-- mission settlement and operator analytics consume one invariant.

CREATE OR REPLACE FUNCTION latarnik_mission_funnel(
    p_workspace_id uuid,
    p_mission_id uuid,
    p_as_of timestamptz
)
RETURNS TABLE (
    mission_id uuid,
    role_id uuid,
    action_id uuid,
    referrer_fan_id uuid,
    anonymous_visitor_id uuid,
    clicked_at timestamptz,
    outcome_deadline timestamptz,
    referred_fan_id uuid,
    referral_code_id uuid,
    referral_status text,
    accepted_at timestamptz,
    qualified_at timestamptz
)
LANGUAGE sql
STABLE
AS $$
    WITH mission AS (
        SELECT
            m.id,
            m.role_id,
            m.action_id,
            m.smart_link_id,
            m.tapped_at,
            m.expires_at,
            fan.id AS referrer_fan_id
        FROM latarnik_missions AS m
        JOIN latarnik_roles AS role
          ON role.workspace_id = m.workspace_id
         AND role.id = m.role_id
        JOIN person_identities AS identity
          ON identity.workspace_id = role.workspace_id
         AND identity.person_id = role.person_id
         AND identity.kind = 'email'
         AND identity.platform IS NULL
        JOIN fans AS fan
          ON fan.workspace_id = identity.workspace_id
         AND fan.normalized_email = identity.value
        WHERE m.workspace_id = p_workspace_id
          AND m.id = p_mission_id
          AND m.tapped_at IS NOT NULL
    ),
    human_clicks AS (
        SELECT
            mission.id AS mission_id,
            mission.role_id,
            mission.action_id,
            mission.referrer_fan_id,
            click.anonymous_visitor_id,
            min(click.occurred_at) AS clicked_at,
            max(mission.expires_at) AS expires_at
        FROM mission
        JOIN smart_links AS link
          ON link.workspace_id = p_workspace_id
         AND link.id = mission.smart_link_id
         AND link.action_id = mission.action_id
        JOIN click_events AS click
          ON click.workspace_id = link.workspace_id
         AND click.smart_link_id = link.id
         AND click.anonymous_visitor_id IS NOT NULL
        WHERE click.occurred_at >= mission.tapped_at
          AND click.occurred_at <= mission.expires_at + interval '7 days'
          AND click.occurred_at <= p_as_of
        GROUP BY
            mission.id,
            mission.role_id,
            mission.action_id,
            mission.referrer_fan_id,
            click.anonymous_visitor_id
    )
    SELECT
        click.mission_id,
        click.role_id,
        click.action_id,
        click.referrer_fan_id,
        click.anonymous_visitor_id,
        click.clicked_at,
        click.expires_at + interval '7 days' AS outcome_deadline,
        arrival.fan_id AS referred_fan_id,
        arrival.referral_code_id,
        referral.status AS referral_status,
        referral.accepted_at,
        referral.qualified_at
    FROM human_clicks AS click
    LEFT JOIN LATERAL (
        SELECT
            acquisition.fan_id,
            acquisition.referral_code_id,
            acquisition.occurred_at
        FROM fan_acquisition_events AS acquisition
        JOIN referral_codes AS code
          ON code.workspace_id = acquisition.workspace_id
         AND code.id = acquisition.referral_code_id
         AND code.fan_id = click.referrer_fan_id
        WHERE acquisition.workspace_id = p_workspace_id
          AND acquisition.anonymous_visitor_id = click.anonymous_visitor_id
          AND acquisition.referrer_fan_id = click.referrer_fan_id
          AND acquisition.fan_id IS NOT NULL
          AND acquisition.referral_code_id IS NOT NULL
          AND acquisition.occurred_at >= click.clicked_at
          AND acquisition.occurred_at <= click.expires_at + interval '7 days'
          AND acquisition.occurred_at <= p_as_of
        ORDER BY acquisition.occurred_at, acquisition.request_id
        LIMIT 1
    ) AS arrival ON true
    LEFT JOIN referral_attributions AS referral
      ON referral.workspace_id = p_workspace_id
     AND referral.referrer_fan_id = click.referrer_fan_id
     AND referral.referred_fan_id = arrival.fan_id
     AND referral.referral_code_id = arrival.referral_code_id
     AND referral.accepted_at >= arrival.occurred_at
     AND referral.accepted_at <= click.expires_at + interval '7 days'
     AND referral.accepted_at <= p_as_of;
$$;

COMMENT ON FUNCTION latarnik_mission_funnel(uuid, uuid, timestamptz) IS
'Exact Latarnik mission funnel: action-owned human click -> same visitor signup/referrer -> same referral attribution. Timing without visitor/action identity never qualifies.';
