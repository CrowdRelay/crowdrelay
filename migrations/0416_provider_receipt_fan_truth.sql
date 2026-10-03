-- FAN_100 integrity: a status/timestamp is not an external publication receipt.
--
-- #545 made the owned-social executor and canonical funnel provider-receipt aware,
-- but the monthly cohort copied an older publication CTE. That left a semantic
-- split where a malformed or hand-edited posted_at could verify a FAN_100 fan
-- while the funnel itself still considered publication unverified.
--
-- Keep Latarnik mission cards as first-party publications: their outward proof
-- remains the action-owned tracked friend click. Every external post table,
-- however, must carry the durable identifier/URL its real executor receives.
--
CREATE OR REPLACE FUNCTION organic_fan_cohort(
    w uuid,
    since_at timestamptz,
    end_at timestamptz,
    as_of timestamptz
)
RETURNS TABLE(
    fan_id uuid,
    action_id uuid,
    link_id uuid,
    acquired_at timestamptz,
    contactable boolean,
    excluded boolean,
    verified boolean
)
LANGUAGE sql STABLE AS $$
WITH RECURSIVE family(root_id, member_id) AS (
    SELECT id,id FROM fans WHERE workspace_id=w AND merged_into_fan_id IS NULL
    UNION
    SELECT family.root_id,child.id FROM family JOIN fans child
      ON child.workspace_id=w AND child.merged_into_fan_id=family.member_id
), identities AS (
    SELECT family.root_id,array_agg(member.id) AS member_ids,MIN(member.created_at) AS first_seen,
      bool_or(exclusion.fan_id IS NOT NULL OR EXISTS(SELECT 1 FROM fan_acquisition_events arrival
        WHERE arrival.workspace_id=w AND arrival.fan_id=member.id
          AND (arrival.source LIKE 'fan_import:%' OR arrival.source='fanbase_ingest'))) AS excluded
    FROM family JOIN fans member ON member.workspace_id=w AND member.id=family.member_id
    LEFT JOIN organic_fan_exclusions exclusion ON exclusion.workspace_id=w AND exclusion.fan_id=member.id
    GROUP BY family.root_id
    HAVING MIN(member.created_at)>=since_at AND MIN(member.created_at)<end_at
      AND MIN(member.created_at)<=as_of
), publications AS (
    SELECT link.id AS link_id,post.action_id,post.posted_at FROM social_posts post
    JOIN smart_links link ON link.workspace_id=w AND (post.smart_link_id=link.id OR post.smart_link='/l/'||link.slug)
    WHERE post.workspace_id=w
      AND post.status='posted'
      AND post.posted_at IS NOT NULL
      AND post.posted_at<=as_of
      AND COALESCE(NULLIF(btrim(post.platform_post_id),''),NULLIF(btrim(post.platform_post_url),'')) IS NOT NULL
    UNION ALL
    SELECT link.id,post.action_id,post.posted_at FROM telegram_posts post
    JOIN smart_links link ON link.workspace_id=w AND (post.smart_link_id=link.id OR post.smart_link='/l/'||link.slug)
    WHERE post.workspace_id=w
      AND post.status='posted'
      AND post.posted_at IS NOT NULL
      AND post.posted_at<=as_of
      AND post.message_id IS NOT NULL
    UNION ALL
    SELECT link.id,post.action_id,post.posted_at FROM discord_posts post
    JOIN smart_links link ON link.workspace_id=w AND (post.smart_link_id=link.id OR post.smart_link='/l/'||link.slug)
    WHERE post.workspace_id=w
      AND post.status='posted'
      AND post.posted_at IS NOT NULL
      AND post.posted_at<=as_of
      AND NULLIF(btrim(post.message_id),'') IS NOT NULL
    UNION ALL
    SELECT link.id,post.action_id,post.posted_at FROM community_posts post
    JOIN smart_links link ON link.workspace_id=w AND post.smart_link='/l/'||link.slug
    WHERE post.workspace_id=w
      AND post.status='posted'
      AND post.posted_at IS NOT NULL
      AND post.posted_at<=as_of
      AND COALESCE(NULLIF(btrim(post.reddit_post_id),''),NULLIF(btrim(post.reddit_post_url),'')) IS NOT NULL
    UNION ALL
    SELECT mission.smart_link_id, mission.action_id, mission.offered_at
    FROM latarnik_missions mission
    WHERE mission.workspace_id=w
      AND mission.action_id IS NOT NULL
      AND mission.smart_link_id IS NOT NULL
      AND mission.offered_at<=as_of
)
SELECT fan.id,credit.action_id,link.id,identity.first_seen,
    fan.status='active' AND fan.deleted_at IS NULL AND COALESCE((SELECT consent.granted FROM fan_consents consent
      WHERE consent.workspace_id=w AND consent.fan_id=fan.id AND consent.purpose='marketing' AND consent.recorded_at<=as_of
      ORDER BY consent.recorded_at DESC,consent.id DESC LIMIT 1),false),
    identity.excluded,
    credit.action_id IS NOT NULL AND link.id IS NOT NULL
      AND (link.action_id IS NULL OR link.action_id=credit.action_id)
      AND EXISTS(SELECT 1 FROM publications p WHERE p.link_id=link.id AND p.action_id=credit.action_id AND p.posted_at<=identity.first_seen)
      AND NOT EXISTS(SELECT 1 FROM publications p WHERE p.link_id=link.id AND p.action_id IS DISTINCT FROM credit.action_id)
      AND EXISTS(SELECT 1 FROM fan_acquisition_events arrival JOIN click_events click
        ON click.workspace_id=w AND click.anonymous_visitor_id=arrival.anonymous_visitor_id AND click.smart_link_id=link.id
        WHERE arrival.workspace_id=w AND arrival.fan_id=ANY(identity.member_ids)
          AND click.occurred_at<=identity.first_seen AND click.occurred_at>=identity.first_seen-INTERVAL '30 days'
          AND EXISTS(SELECT 1 FROM publications p WHERE p.link_id=link.id AND p.action_id=credit.action_id AND p.posted_at<=click.occurred_at))
FROM identities identity JOIN fans fan ON fan.workspace_id=w AND fan.id=identity.root_id
LEFT JOIN LATERAL (
    SELECT conversion.action_id,conversion.source_target FROM fan_provenance_events conversion
    WHERE conversion.workspace_id=w AND conversion.fan_id=ANY(identity.member_ids)
      AND conversion.event_kind='conversion' AND conversion.attribution_method='last_tracked_click'
      AND conversion.occurred_at=identity.first_seen
    ORDER BY conversion.occurred_at,conversion.id LIMIT 1
) credit ON true
LEFT JOIN smart_links link ON link.workspace_id=w AND link.slug=credit.source_target
$$;
