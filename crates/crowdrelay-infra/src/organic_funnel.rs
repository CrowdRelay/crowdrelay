//! Live canonical fan cohorts. Traffic is diagnostic evidence, never causal lift.
use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct OrganicFunnelRow {
    pub link_id: Uuid,
    pub slug: String,
    pub campaign_id: Option<Uuid>,
    pub action_id: Option<Uuid>,
    pub channel: Option<String>,
    pub active: bool,
    pub ambiguous_owner: bool,
    #[serde(with = "time::serde::rfc3339::option")]
    pub published_at: Option<OffsetDateTime>,
    pub unique_visitors: i64,
    pub signups: i64,
    pub confirmed: i64,
    pub activated: i64,
    pub activation_mature: i64,
    pub activated_mature: i64,
    pub retention_mature: i64,
    pub retained: i64,
    pub qualified_referrals: i64,
    pub diagnosis: String,
}

pub async fn read(pool: &PgPool, workspace: Uuid, action: Option<Uuid>, campaign: Option<Uuid>,
    days: i32, limit: i64, now: OffsetDateTime) -> Result<Vec<OrganicFunnelRow>,sqlx::Error> {
    sqlx::query_as(FUNNEL_SQL).bind(workspace).bind(action).bind(campaign)
        .bind(days.clamp(1,90)).bind(limit.clamp(1,100)).bind(now).fetch_all(pool).await
}

const FUNNEL_SQL: &str = r#"
WITH publications AS (
 SELECT smart_link_id AS link_id,action_id,posted_at FROM social_posts WHERE workspace_id=$1 AND smart_link_id IS NOT NULL AND posted_at IS NOT NULL
 UNION ALL SELECT smart_link_id,action_id,posted_at FROM telegram_posts WHERE workspace_id=$1 AND smart_link_id IS NOT NULL AND posted_at IS NOT NULL
 UNION ALL SELECT smart_link_id,action_id,posted_at FROM discord_posts WHERE workspace_id=$1 AND smart_link_id IS NOT NULL AND posted_at IS NOT NULL
 UNION ALL SELECT link.id,post.action_id,post.posted_at FROM community_posts post
 JOIN smart_links link ON link.workspace_id=post.workspace_id AND post.smart_link='/l/'||link.slug
 WHERE post.workspace_id=$1 AND post.posted_at IS NOT NULL
), links AS MATERIALIZED (
 SELECT link.id,link.slug,link.campaign_id,link.channel_source,link.active,
   COALESCE(link.action_id,publication.action_id) AS action_id,publication.posted_at AS published_at,
   (SELECT COUNT(DISTINCT action_id)>1 OR COALESCE(bool_or(link.action_id IS NOT NULL AND action_id IS DISTINCT FROM link.action_id),false)
    FROM publications WHERE link_id=link.id) AS ambiguous_owner
 FROM smart_links link LEFT JOIN LATERAL (
   SELECT action_id,MIN(posted_at) AS posted_at FROM publications WHERE link_id=link.id
   GROUP BY action_id ORDER BY MIN(posted_at),action_id LIMIT 1
 ) publication ON true
 WHERE link.workspace_id=$1
   AND ($2::uuid IS NULL OR COALESCE(link.action_id,publication.action_id)=$2)
   AND ($3::uuid IS NULL OR link.campaign_id=$3)
   AND ($2::uuid IS NOT NULL OR $3::uuid IS NOT NULL OR link.created_at >= $6-make_interval(days=>$4)
        OR publication.posted_at >= $6-make_interval(days=>$4)
        OR EXISTS(SELECT 1 FROM click_events c WHERE c.workspace_id=$1 AND c.smart_link_id=link.id AND c.occurred_at >= $6-make_interval(days=>$4)))
 ORDER BY publication.posted_at DESC NULLS LAST,link.created_at DESC,link.id LIMIT $5
), cohorts AS MATERIALIZED (
 SELECT link.id AS link_id,fan.id AS fan_id,conversion.occurred_at AS acquired_at,
   fan.status='active' AND fan.deleted_at IS NULL AND COALESCE((SELECT granted FROM fan_consents
     WHERE workspace_id=$1 AND fan_id=fan.id AND purpose='marketing' AND recorded_at <= $6
     ORDER BY recorded_at DESC,id DESC LIMIT 1),false) AS contactable,
   fan_has_engagement_between($1,fan.id,fan.normalized_email,conversion.occurred_at+INTERVAL '1 microsecond',
     LEAST(conversion.occurred_at+INTERVAL '7 days',$6+INTERVAL '1 microsecond')) AS engaged,
   fan_is_meaningfully_retained($1,fan.id,conversion.occurred_at,$6)
     AND fan_has_engagement_between($1,fan.id,fan.normalized_email,
       GREATEST(conversion.occurred_at+INTERVAL '30 days',$6-INTERVAL '30 days'),$6+INTERVAL '1 microsecond') AS retained
 FROM links link JOIN fan_provenance_events conversion ON conversion.workspace_id=$1
   AND conversion.event_kind='conversion' AND conversion.attribution_method='last_tracked_click'
   AND conversion.source_target=link.slug AND conversion.action_id IS NOT DISTINCT FROM link.action_id
 JOIN fans fan ON fan.workspace_id=$1 AND fan.id=conversion.fan_id
 WHERE NOT link.ambiguous_owner AND conversion.occurred_at >= $6-make_interval(days=>$4) AND conversion.occurred_at <= $6
   AND (link.published_at IS NULL OR conversion.occurred_at >= link.published_at)
), counts AS (
 SELECT link.id,
   (SELECT COUNT(DISTINCT anonymous_visitor_id) FROM click_events WHERE workspace_id=$1 AND smart_link_id=link.id
     AND occurred_at >= $6-make_interval(days=>$4) AND occurred_at <= $6
     AND (link.published_at IS NULL OR occurred_at >= link.published_at)) AS unique_visitors,
   cohort.*,
   (SELECT COUNT(DISTINCT referral.referred_fan_id) FROM cohorts c JOIN referral_attributions referral
     ON referral.workspace_id=$1 AND referral.referrer_fan_id=c.fan_id WHERE c.link_id=link.id AND referral.status='qualified'
     AND referral.qualified_at>c.acquired_at AND referral.qualified_at <= $6) AS qualified_referrals
 FROM links link CROSS JOIN LATERAL (
   SELECT COUNT(DISTINCT fan_id) AS signups,
     COUNT(DISTINCT fan_id) FILTER(WHERE contactable) AS confirmed,
     COUNT(DISTINCT fan_id) FILTER(WHERE contactable AND engaged) AS activated,
     COUNT(DISTINCT fan_id) FILTER(WHERE acquired_at <= $6-INTERVAL '7 days') AS activation_mature,
     COUNT(DISTINCT fan_id) FILTER(WHERE contactable AND engaged AND acquired_at <= $6-INTERVAL '7 days') AS activated_mature,
     COUNT(DISTINCT fan_id) FILTER(WHERE acquired_at <= $6-INTERVAL '30 days') AS retention_mature,
     COUNT(DISTINCT fan_id) FILTER(WHERE contactable AND retained) AS retained
   FROM cohorts WHERE link_id=link.id
 ) cohort
)
SELECT link.id AS link_id,link.slug,link.campaign_id,link.action_id,link.channel_source AS channel,
 link.active,link.ambiguous_owner,link.published_at,counts.unique_visitors,counts.signups,counts.confirmed,
 counts.activated,counts.activation_mature,counts.activated_mature,counts.retention_mature,counts.retained,counts.qualified_referrals,
 CASE WHEN NOT link.active THEN 'inactive_link' WHEN link.ambiguous_owner THEN 'ambiguous_link_owner'
   WHEN link.published_at IS NULL THEN 'publication_unverified' WHEN link.published_at>$6-INTERVAL '1 day' THEN 'awaiting_traffic_window'
   WHEN counts.unique_visitors=0 THEN 'no_observed_visitors' WHEN counts.signups=0 THEN 'visitors_without_signup'
   WHEN counts.confirmed=0 THEN 'signup_without_confirmation' WHEN counts.activation_mature=0 THEN 'awaiting_activation_window'
   WHEN counts.activated_mature=0 THEN 'confirmed_without_activation' WHEN counts.retention_mature=0 THEN 'awaiting_retention_window'
   WHEN counts.retained=0 THEN 'no_observed_retention' ELSE 'retained_fans_observed' END AS diagnosis
FROM links link JOIN counts ON counts.id=link.id
"#;
