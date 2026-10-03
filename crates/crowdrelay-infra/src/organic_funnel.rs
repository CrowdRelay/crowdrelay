//! Live canonical fan cohorts. Traffic is diagnostic evidence, never causal lift.
use serde::Serialize;
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crowdrelay_application::autopilot::{
    ConfirmationRecoverySnapshot, OrganicFunnelControl, OrganicFunnelDirective,
};
use crowdrelay_domain::{FanId, action_ledger::provider_delivery_set_is_definitive_failure};

#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
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

pub async fn read(
    pool: &PgPool,
    workspace: Uuid,
    action: Option<Uuid>,
    campaign: Option<Uuid>,
    days: i32,
    limit: i64,
    now: OffsetDateTime,
) -> Result<Vec<OrganicFunnelRow>, sqlx::Error> {
    sqlx::query_as(FUNNEL_SQL)
        .bind(workspace)
        .bind(action)
        .bind(campaign)
        .bind(days.clamp(1, 90))
        .bind(limit.clamp(1, 100))
        .bind(now)
        .fetch_all(pool)
        .await
}

pub async fn control(
    pool: &PgPool,
    workspace: Uuid,
    now: OffsetDateTime,
) -> Result<Option<OrganicFunnelControl>, sqlx::Error> {
    let rows = read(pool, workspace, None, None, 90, 100, now).await?;
    Ok(derive_control(&rows, now))
}

fn derive_control(rows: &[OrganicFunnelRow], now: OffsetDateTime) -> Option<OrganicFunnelControl> {
    let mature_before = now - Duration::days(1);
    let recent_after = now - Duration::days(30);
    let mature: Vec<&OrganicFunnelRow> = rows
        .iter()
        .filter(|row| {
            row.active
                && row.action_id.is_some()
                && !row.ambiguous_owner
                && row.published_at.is_some_and(|at| at <= mature_before)
        })
        .collect();
    if mature.is_empty() {
        return None;
    }
    let recent: Vec<&OrganicFunnelRow> = mature
        .iter()
        .copied()
        .filter(|row| row.published_at.is_some_and(|at| at >= recent_after))
        .collect();

    let sum_rows = |set: &[&OrganicFunnelRow], value: fn(&OrganicFunnelRow) -> i64| -> u32 {
        let total = set
            .iter()
            .fold(0_i64, |acc, row| acc.saturating_add(value(row).max(0)));
        u32::try_from(total).unwrap_or(u32::MAX)
    };

    let unique_visitors = sum_rows(&recent, |row| row.unique_visitors);
    let signups = sum_rows(&recent, |row| row.signups);
    let confirmed = sum_rows(&recent, |row| row.confirmed);
    let activation_mature = sum_rows(&mature, |row| row.activation_mature);
    let activated_mature = sum_rows(&mature, |row| row.activated_mature);
    let retention_mature = sum_rows(&mature, |row| row.retention_mature);
    let retained = sum_rows(&mature, |row| row.retained);
    let qualified_referrals = sum_rows(&mature, |row| row.qualified_referrals);

    let directive = if !recent.is_empty() && unique_visitors == 0 {
        OrganicFunnelDirective::ExpandReach
    } else if !recent.is_empty() && signups == 0 {
        OrganicFunnelDirective::RepairConversion
    } else if !recent.is_empty() && confirmed == 0 {
        OrganicFunnelDirective::RepairConfirmation
    } else if activation_mature > 0 && activated_mature == 0 {
        OrganicFunnelDirective::ActivateFans
    } else if retention_mature > 0 && retained == 0 {
        OrganicFunnelDirective::RetainFans
    } else if retained > 0 && qualified_referrals == 0 {
        OrganicFunnelDirective::MultiplyReferrals
    } else {
        return None;
    };

    Some(OrganicFunnelControl {
        directive,
        mature_links: u32::try_from(mature.len()).unwrap_or(u32::MAX),
        unique_visitors,
        signups,
        confirmed,
        activation_mature,
        activated_mature,
        retention_mature,
        retained,
        qualified_referrals,
    })
}

const CONFIRMATION_RECOVERY_MIN_AGE_HOURS: i64 = 1;
const CONFIRMATION_RECOVERY_LOOKBACK_DAYS: i64 = 30;
const CONFIRMATION_RECOVERY_LIMIT: i64 = 50;

/// Pending, CrowdRelay-attributed signups whose latest double-opt-in delivery
/// definitively failed.
///
/// A missing confirmation is not enough. The latest confirmation event must
/// be old enough to have left the normal delivery window and the provider
/// evidence must prove a definitive failure. Ambiguous transport loss is never
/// enough to rotate a token: the provider may already have accepted the mail.
/// A delivered or still-in-flight route always blocks autonomous recovery.
pub async fn confirmation_recovery_snapshots(
    pool: &PgPool,
    workspace: Uuid,
    now: OffsetDateTime,
) -> Result<Vec<ConfirmationRecoverySnapshot>, sqlx::Error> {
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        Uuid,
        Uuid,
        String,
        OffsetDateTime,
        Uuid,
        OffsetDateTime,
        String,
        Option<String>,
        i64,
        i64,
        Vec<String>,
    )> = sqlx::query_as(
        r#"
        WITH attributed AS (
            SELECT DISTINCT ON (conversion.fan_id)
                conversion.fan_id,
                conversion.action_id AS source_action_id,
                conversion.source_target,
                conversion.occurred_at AS acquired_at
            FROM fan_provenance_events AS conversion
            JOIN fans AS fan
              ON fan.workspace_id = conversion.workspace_id
             AND fan.id = conversion.fan_id
            WHERE conversion.workspace_id = $1
              AND conversion.event_kind = 'conversion'
              AND conversion.attribution_method = 'last_tracked_click'
              AND conversion.action_id IS NOT NULL
              AND conversion.source_target IS NOT NULL
              AND btrim(conversion.source_target) <> ''
              AND conversion.occurred_at >= $2 - make_interval(days => $3::int)
              AND conversion.occurred_at <= $2
              AND fan.status = 'pending'
              AND fan.deleted_at IS NULL
              AND COALESCE((
                  SELECT consent.granted
                  FROM fan_consents AS consent
                  WHERE consent.workspace_id = fan.workspace_id
                    AND consent.fan_id = fan.id
                    AND consent.purpose = 'marketing'
                    AND consent.recorded_at <= $2
                  ORDER BY consent.recorded_at DESC, consent.id DESC
                  LIMIT 1
              ), false)
            ORDER BY conversion.fan_id, conversion.occurred_at DESC, conversion.id DESC
        ),
        latest_confirmation AS (
            SELECT attributed.*,
                   confirmation.id AS outbox_event_id,
                   confirmation.created_at AS event_created_at,
                   confirmation.status AS event_status,
                   confirmation.last_error_kind AS event_error_kind
            FROM attributed
            JOIN LATERAL (
                SELECT event.id, event.created_at, event.status, event.last_error_kind
                FROM outbox_events AS event
                WHERE event.workspace_id = $1
                  AND event.event_type = 'fan.confirmation_requested'
                  AND event.payload->>'fan_id' = attributed.fan_id::text
                  AND event.created_at >= attributed.acquired_at
                  AND event.created_at <= $2
                ORDER BY event.created_at DESC, event.id DESC
                LIMIT 1
            ) AS confirmation ON true
        )
        SELECT confirmation.fan_id,
               confirmation.source_action_id,
               confirmation.source_target,
               confirmation.acquired_at,
               confirmation.outbox_event_id,
               confirmation.event_created_at,
               confirmation.event_status,
               confirmation.event_error_kind,
               COALESCE(delivery.delivered, 0)::bigint,
               COALESCE(delivery.in_flight, 0)::bigint,
               COALESCE(delivery.terminal_error_kinds, ARRAY[]::text[])
        FROM latest_confirmation AS confirmation
        LEFT JOIN LATERAL (
            SELECT
                count(*) FILTER (WHERE d.status = 'delivered')::bigint AS delivered,
                count(*) FILTER (WHERE d.status IN ('pending','processing'))::bigint AS in_flight,
                array_agg(
                    COALESCE(NULLIF(d.last_error_kind, ''), 'unknown')
                    ORDER BY d.updated_at, d.id
                ) FILTER (WHERE d.status IN ('dead','cancelled')) AS terminal_error_kinds
            FROM webhook_deliveries AS d
            WHERE d.workspace_id = $1
              AND d.outbox_event_id = confirmation.outbox_event_id
        ) AS delivery ON true
        WHERE confirmation.event_created_at <=
                  $2 - make_interval(hours => $4::int)
          -- Exactly one autonomous retry per attributable acquisition
          -- episode. If the retry itself fails, the fan can explicitly
          -- request another access email; the Brain never turns a broken
          -- mail route into a retry loop.
          AND NOT EXISTS (
              SELECT 1
              FROM autopilot_actions AS action
              WHERE action.workspace_id = $1
                AND action.subject_id = confirmation.fan_id
                AND action.action_kind = 'fan.lifecycle.message.request'
                AND action.payload->>'template_key' =
                    'crowdrelay.fan.confirmation_recovery.v1'
                AND action.created_at >= confirmation.acquired_at
          )
        ORDER BY confirmation.event_created_at, confirmation.fan_id
        "#,
    )
    .bind(workspace)
    .bind(now)
    .bind(CONFIRMATION_RECOVERY_LOOKBACK_DAYS)
    .bind(CONFIRMATION_RECOVERY_MIN_AGE_HOURS)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(
            |(
                fan_id,
                source_action_id,
                source_target,
                acquired_at,
                failed_outbox_event_id,
                failed_event_created_at,
                event_status,
                event_error_kind,
                delivered,
                in_flight,
                terminal_error_kinds,
            )| {
                if delivered > 0 || in_flight > 0 {
                    return None;
                }
                if !provider_delivery_set_is_definitive_failure(
                    &event_status,
                    event_error_kind.as_deref(),
                    delivered,
                    in_flight,
                    &terminal_error_kinds,
                ) {
                    return None;
                }
                let failure_kind = event_error_kind
                    .filter(|kind| {
                        provider_delivery_set_is_definitive_failure(
                            &event_status,
                            Some(kind),
                            delivered,
                            in_flight,
                            &[],
                        )
                    })
                    .or_else(|| terminal_error_kinds.last().cloned())
                    .unwrap_or_else(|| "delivery_definitive_failure".to_owned());
                Some(ConfirmationRecoverySnapshot {
                    fan_id: FanId::from_uuid(fan_id),
                    source_action_id,
                    source_target,
                    acquired_at,
                    failed_outbox_event_id,
                    failed_event_created_at,
                    failure_kind,
                })
            },
        )
        .take(usize::try_from(CONFIRMATION_RECOVERY_LIMIT).unwrap_or(50))
        .collect())
}

const FUNNEL_SQL: &str = r#"
WITH publications AS (
 SELECT smart_link_id AS link_id,action_id,posted_at FROM social_posts
 WHERE workspace_id=$1
   AND smart_link_id IS NOT NULL
   AND posted_at IS NOT NULL
   AND status='posted'
   AND COALESCE(NULLIF(btrim(platform_post_id),''),NULLIF(btrim(platform_post_url),'')) IS NOT NULL
 UNION ALL SELECT smart_link_id,action_id,posted_at FROM telegram_posts
 WHERE workspace_id=$1
   AND smart_link_id IS NOT NULL
   AND status='posted'
   AND posted_at IS NOT NULL
   AND message_id IS NOT NULL
 UNION ALL SELECT smart_link_id,action_id,posted_at FROM discord_posts
 WHERE workspace_id=$1
   AND smart_link_id IS NOT NULL
   AND status='posted'
   AND posted_at IS NOT NULL
   AND NULLIF(btrim(message_id),'') IS NOT NULL
 UNION ALL SELECT link.id,post.action_id,post.posted_at FROM community_posts post
 JOIN smart_links link ON link.workspace_id=post.workspace_id AND post.smart_link='/l/'||link.slug
 WHERE post.workspace_id=$1
   AND post.status='posted'
   AND post.posted_at IS NOT NULL
   AND COALESCE(NULLIF(btrim(post.reddit_post_id),''),NULLIF(btrim(post.reddit_post_url),'')) IS NOT NULL
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
   WHEN counts.retained=0 THEN 'no_observed_retention'
   WHEN counts.qualified_referrals=0 THEN 'retained_without_referral'
   ELSE 'referral_multiplication_observed' END AS diagnosis
FROM links link JOIN counts ON counts.id=link.id
"#;

#[cfg(test)]
mod control_tests {
    use super::*;

    fn row(published_at: OffsetDateTime) -> OrganicFunnelRow {
        OrganicFunnelRow {
            link_id: Uuid::now_v7(),
            slug: "test".to_owned(),
            campaign_id: None,
            action_id: Some(Uuid::now_v7()),
            channel: Some("reddit".to_owned()),
            active: true,
            ambiguous_owner: false,
            published_at: Some(published_at),
            unique_visitors: 0,
            signups: 0,
            confirmed: 0,
            activated: 0,
            activation_mature: 0,
            activated_mature: 0,
            retention_mature: 0,
            retained: 0,
            qualified_referrals: 0,
            diagnosis: String::new(),
        }
    }

    #[test]
    fn funnel_control_follows_the_first_zero_after_real_denominators() {
        let now = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
        let mut value = row(now - Duration::days(3));

        assert_eq!(
            derive_control(&[value.clone()], now).map(|c| c.directive),
            Some(OrganicFunnelDirective::ExpandReach)
        );

        value.unique_visitors = 12;
        assert_eq!(
            derive_control(&[value.clone()], now).map(|c| c.directive),
            Some(OrganicFunnelDirective::RepairConversion)
        );

        value.signups = 3;
        assert_eq!(
            derive_control(&[value.clone()], now).map(|c| c.directive),
            Some(OrganicFunnelDirective::RepairConfirmation)
        );

        value.confirmed = 2;
        value.activation_mature = 2;
        assert_eq!(
            derive_control(&[value.clone()], now).map(|c| c.directive),
            Some(OrganicFunnelDirective::ActivateFans)
        );

        value.activated_mature = 1;
        value.retention_mature = 1;
        assert_eq!(
            derive_control(&[value.clone()], now).map(|c| c.directive),
            Some(OrganicFunnelDirective::RetainFans)
        );

        value.retained = 1;
        assert_eq!(
            derive_control(&[value.clone()], now).map(|c| c.directive),
            Some(OrganicFunnelDirective::MultiplyReferrals)
        );

        value.qualified_referrals = 1;
        assert_eq!(derive_control(&[value], now), None);
    }

    #[test]
    fn immature_unverified_and_ambiguous_links_never_drive_control() {
        let now = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
        let recent = row(now - Duration::hours(8));
        let mut ambiguous = row(now - Duration::days(3));
        ambiguous.ambiguous_owner = true;
        let mut unverified = row(now - Duration::days(3));
        unverified.published_at = None;
        let mut unattributed = row(now - Duration::days(3));
        unattributed.action_id = None;

        assert_eq!(
            derive_control(&[recent, ambiguous, unverified, unattributed], now),
            None
        );
    }
}
