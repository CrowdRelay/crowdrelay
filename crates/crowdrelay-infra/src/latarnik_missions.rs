//! Persistence for one-tap Latarnik missions.
//!
//! Reads the facts a mission is made of (the Latarnik's own city, published
//! shows, recent releases, their own referral code), writes the one mission the
//! pure chooser (`crowdrelay_domain::latarnik_mission`) hands back, and measures
//! it by the only thing that counts: a referred person who arrived through the
//! Latarnik's own code after the tap. Every statement names `workspace_id` on
//! every table it reads.
//!
//! The referral link is the stored `member_site_base_url` or nothing — never the
//! shipped default, because a default URL in a text a person sends to a friend
//! is a link to somebody else's website.

use crowdrelay_application::autopilot::AutopilotActionPayload;
use crowdrelay_domain::{
    FanId, TraceContext, WorkspaceId,
    latarnik_mission::{
        AdvocacyYield, Language, MISSION_LIFETIME, MissionContext, MissionPlan, ReleaseFact,
        SHOW_HORIZON, ShowFact,
    },
};
use serde_json::json;
use sqlx::{FromRow, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::latarnik_roles::LatarnikError;

#[derive(Debug, FromRow)]
struct CarrierRow {
    role_id: Uuid,
    fan_id: Uuid,
    locale: Option<String>,
    last_offered_at: Option<OffsetDateTime>,
    has_open_mission: bool,
    referral_code: Option<String>,
    offered_90d: i64,
    tapped_90d: i64,
    human_clickers_90d: i64,
    completed_90d: i64,
    seen_event_ids: Vec<Uuid>,
    seen_content_source_ids: Vec<Uuid>,
}

/// An active Latarnik and the facts their next mission would be made of.
#[derive(Debug)]
pub struct Carrier {
    pub role_id: Uuid,
    pub fan_id: Uuid,
    pub context: MissionContext,
}

/// Active Latarnik roles that carry the referral capability, with their context.
///
/// # Errors
///
/// Propagates the database error.
pub async fn load_carriers(
    pool: &PgPool,
    workspace_id: Uuid,
    now: OffsetDateTime,
    limit: i64,
) -> Result<Vec<Carrier>, LatarnikError> {
    let site_root = sqlx::query_scalar::<_, String>(
        "SELECT value FROM tenant_settings
         WHERE workspace_id = $1 AND key = 'member_site_base_url'",
    )
    .bind(workspace_id)
    .fetch_optional(pool)
    .await?
    .map(|value| value.trim().trim_end_matches('/').to_owned())
    .filter(|value| !value.is_empty());

    let rows = sqlx::query_as::<_, CarrierRow>(
        "SELECT lr.id AS role_id,
                fan.id AS fan_id,
                fan.locale,
                history.last_offered_at,
                history.has_open_mission,
                (SELECT rc.code FROM referral_codes rc
                  WHERE rc.workspace_id = fan.workspace_id AND rc.fan_id = fan.id AND rc.active
                  ORDER BY rc.created_at, rc.id LIMIT 1) AS referral_code,
                history.offered_90d,
                history.tapped_90d,
                COALESCE((
                    SELECT count(DISTINCT provenance.anonymous_visitor_id)::bigint
                    FROM fan_provenance_events provenance
                    WHERE provenance.workspace_id = lr.workspace_id
                      AND provenance.event_kind = 'interaction'
                      AND provenance.channel = 'referral'
                      AND provenance.attribution_method = 'referral_click'
                      AND provenance.anonymous_visitor_id IS NOT NULL
                      AND provenance.source_target = 'fan:' || fan.id::text
                      AND EXISTS (
                          SELECT 1
                          FROM latarnik_missions measured
                          WHERE measured.workspace_id = lr.workspace_id
                            AND measured.role_id = lr.id
                            AND measured.offered_at >= $3 - interval '90 days'
                            AND measured.tapped_at IS NOT NULL
                            AND provenance.occurred_at >= measured.tapped_at
                            AND provenance.occurred_at
                                <= measured.expires_at + interval '7 days'
                      )
                ), 0)::bigint AS human_clickers_90d,
                history.completed_90d,
                history.seen_event_ids,
                history.seen_content_source_ids
         FROM latarnik_roles lr
         JOIN person_identities pi
           ON pi.workspace_id = lr.workspace_id AND pi.person_id = lr.person_id
          AND pi.kind = 'email' AND pi.platform IS NULL
         JOIN fans fan
           ON fan.workspace_id = pi.workspace_id AND fan.normalized_email = pi.value
          AND fan.status = 'active' AND fan.deleted_at IS NULL
          AND fan.merged_into_fan_id IS NULL
         LEFT JOIN LATERAL (
             SELECT
                 max(m.offered_at) AS last_offered_at,
                 COALESCE(bool_or(m.status IN ('offered', 'tapped')), false)
                     AS has_open_mission,
                 count(*) FILTER (
                     WHERE m.offered_at >= $3 - interval '90 days'
                 )::bigint AS offered_90d,
                 count(*) FILTER (
                     WHERE m.offered_at >= $3 - interval '90 days'
                       AND m.tapped_at IS NOT NULL
                 )::bigint AS tapped_90d,
                 count(*) FILTER (
                     WHERE m.offered_at >= $3 - interval '90 days'
                       AND m.status = 'completed'
                 )::bigint AS completed_90d,
                 COALESCE(
                     array_agg(DISTINCT m.event_id)
                         FILTER (WHERE m.event_id IS NOT NULL),
                     ARRAY[]::uuid[]
                 ) AS seen_event_ids,
                 COALESCE(
                     array_agg(DISTINCT m.content_source_id)
                         FILTER (WHERE m.content_source_id IS NOT NULL),
                     ARRAY[]::uuid[]
                 ) AS seen_content_source_ids
             FROM latarnik_missions m
             WHERE m.workspace_id = lr.workspace_id
               AND m.role_id = lr.id
         ) history ON true
         WHERE lr.workspace_id = $1
           AND lr.status = 'active'
           AND lr.capabilities ? 'referral_link'
         ORDER BY lr.activated_at, lr.id
         LIMIT $2",
    )
    .bind(workspace_id)
    .bind(limit)
    .bind(now)
    .fetch_all(pool)
    .await?;

    // Shows and releases are the same for every Latarnik; their *city* is not.
    let shows = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            String,
            Option<Uuid>,
            Option<String>,
            OffsetDateTime,
        ),
    >(
        "SELECT e.id, e.title, e.slug, e.city_id, c.name, e.starts_at
         FROM events e
         LEFT JOIN cities c ON c.id = e.city_id
         WHERE e.workspace_id = $1
           AND e.status = 'published'
           AND e.starts_at > $2
           AND e.starts_at <= $2 + make_interval(secs => $3)
         ORDER BY e.starts_at
         LIMIT 50",
    )
    .bind(workspace_id)
    .bind(now)
    .bind(SHOW_HORIZON.whole_seconds() as f64)
    .fetch_all(pool)
    .await?;
    let releases = sqlx::query_as::<_, (Uuid, String, OffsetDateTime)>(
        "SELECT id, title, occurred_at
         FROM content_sources
         WHERE workspace_id = $1
           AND source_kind IN ('release', 'video')
           AND active
           AND occurred_at <= $2
         ORDER BY occurred_at DESC
         LIMIT 20",
    )
    .bind(workspace_id)
    .bind(now)
    .fetch_all(pool)
    .await?;

    let mut carriers = Vec::with_capacity(rows.len());
    for row in rows {
        let cities: Vec<Uuid> = sqlx::query_scalar(
            "SELECT city_id FROM fan_city_interests WHERE workspace_id = $1 AND fan_id = $2",
        )
        .bind(workspace_id)
        .bind(row.fan_id)
        .fetch_all(pool)
        .await?;
        let referral_url = match (&site_root, &row.referral_code) {
            (Some(root), Some(code)) => Some(format!("{root}/r/{code}")),
            _ => None,
        };
        carriers.push(Carrier {
            role_id: row.role_id,
            fan_id: row.fan_id,
            context: MissionContext {
                may_carry: true,
                has_open_mission: row.has_open_mission,
                last_offered_at: row.last_offered_at,
                advocacy_yield: AdvocacyYield {
                    offered_90d: u32::try_from(row.offered_90d).unwrap_or(u32::MAX),
                    tapped_90d: u32::try_from(row.tapped_90d).unwrap_or(u32::MAX),
                    human_clickers_90d: u32::try_from(row.human_clickers_90d).unwrap_or(u32::MAX),
                    completed_90d: u32::try_from(row.completed_90d).unwrap_or(u32::MAX),
                },
                seen_event_ids: row.seen_event_ids,
                seen_content_source_ids: row.seen_content_source_ids,
                language: Language::from_locale(row.locale.as_deref()),
                referral_url,
                shows: shows
                    .iter()
                    .map(
                        |(event_id, title, slug, city_id, city, starts_at)| ShowFact {
                            event_id: *event_id,
                            slug: slug.clone(),
                            title: title.clone(),
                            city: city.clone(),
                            starts_on: starts_at.date(),
                            starts_at: *starts_at,
                            in_their_city: city_id.is_some_and(|id| cities.contains(&id)),
                        },
                    )
                    .collect(),
                releases: releases
                    .iter()
                    .map(|(id, title, at)| ReleaseFact {
                        content_source_id: *id,
                        title: title.clone(),
                        published_at: *at,
                    })
                    .collect(),
            },
        });
    }
    Ok(carriers)
}

/// Writes the chosen mission. The partial unique index is the arbiter: a second
/// open mission for the same role is a no-op, not an error. Returns the id only
/// when this call created it.
///
/// # Errors
///
/// Propagates the database error.
pub async fn offer(
    pool: &PgPool,
    workspace_id: Uuid,
    role_id: Uuid,
    fan_id: Uuid,
    plan: &MissionPlan,
    now: OffsetDateTime,
) -> Result<Option<Uuid>, LatarnikError> {
    // Re-read the trusted first-party root at the write seam. The chooser
    // built the direct referral destination from this setting, but settings
    // can change between its read and this transaction.
    let mut tx = pool.begin().await?;
    let Some(site_root) = sqlx::query_scalar::<_, String>(
        "SELECT value FROM tenant_settings
         WHERE workspace_id = $1 AND key = 'member_site_base_url'",
    )
    .bind(workspace_id)
    .fetch_optional(&mut *tx)
    .await?
    .map(|value| value.trim().trim_end_matches('/').to_owned())
    .filter(|value| !value.is_empty())
    else {
        tx.rollback().await?;
        return Ok(None);
    };
    if !plan
        .destination_url
        .starts_with(&format!("{site_root}/r/"))
        || !plan.share_text.contains(&plan.destination_url)
    {
        tx.rollback().await?;
        return Ok(None);
    }

    // The mission itself is a first-party intervention. Give it an action at
    // the same instant it becomes visible so later acquisition credit has a
    // real owner rather than a synthetic campaign id.
    let anchor = plan
        .event_id
        .or(plan.content_source_id)
        .unwrap_or(role_id);
    let decision_key = format!(
        "latarnik.mission:{}:{}:{}",
        role_id,
        plan.kind.as_str(),
        anchor
    );
    let idempotency_key = format!(
        "latarnik-mission:{}:{}:{}",
        role_id,
        plan.kind.as_str(),
        anchor
    );
    let action_id = Uuid::now_v7();
    let payload = AutopilotActionPayload::OfferLatarnikMission {
        role_id,
        fan_id: FanId::from_uuid(fan_id),
        mission_kind: plan.kind.as_str().to_owned(),
        event_id: plan.event_id,
        content_source_id: plan.content_source_id,
        prompt: plan.prompt.clone(),
        share_text: plan.share_text.clone(),
        destination_url: plan.destination_url.clone(),
    };
    let payload_json = serde_json::to_value(&payload)
        .map_err(|error| LatarnikError::Database(sqlx::Error::Protocol(error.to_string())))?;
    let trace = TraceContext::root(WorkspaceId::from_uuid(workspace_id));
    let decision_id = match sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id
        ) VALUES (
            $1,$2,$3,'fan_lifecycle','fan',$4,'latarnik.mission.offer',
            10000,'auto_execute',
            'Active Latarnik has one fresh, bounded referral mission',
            $5,$6,$7,$8,$9
        )
        ON CONFLICT (workspace_id, decision_key) DO NOTHING
        RETURNING id
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id)
    .bind(&decision_key)
    .bind(fan_id)
    .bind(json!({
        "role_id": role_id,
        "mission_kind": plan.kind.as_str(),
        "event_id": plan.event_id,
        "content_source_id": plan.content_source_id,
    }))
    .bind(json!({
        "surface": "authenticated_signal",
        "one_open_mission": true,
        "one_person_ask": true,
        "outbound_send": false,
    }))
    .bind(&payload_json)
    .bind(now)
    .bind(trace.trace_id().into_uuid())
    .fetch_optional(&mut *tx)
    .await?
    {
        Some(id) => id,
        None => {
            let existing = sqlx::query_as::<_, (Uuid, Uuid)>(
                "SELECT decision.id, action.id
                 FROM autopilot_decisions decision
                 JOIN autopilot_actions action
                   ON action.workspace_id = decision.workspace_id
                  AND action.decision_id = decision.id
                 WHERE decision.workspace_id = $1
                   AND decision.decision_key = $2
                   AND action.idempotency_key = $3
                 LIMIT 1",
            )
            .bind(workspace_id)
            .bind(&decision_key)
            .bind(&idempotency_key)
            .fetch_optional(&mut *tx)
            .await?;
            tx.rollback().await?;
            return Ok(existing.map(|(_, existing_action)| existing_action));
        }
    };

    let action_trace = TraceContext::for_action(
        WorkspaceId::from_uuid(workspace_id),
        trace.trace_id(),
        action_id,
        Some(decision_id),
    );
    let slug = format!("latarnik-{}", action_id.simple());
    let tracked_url = format!("{site_root}/l/{slug}");
    let share_text = plan
        .share_text
        .replacen(&plan.destination_url, &tracked_url, 1);

    let smart_link_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO smart_links (
            workspace_id, slug, destination_url, active, channel_source, action_id
        ) VALUES ($1,$2,$3,true,'latarnik',$4)
        ON CONFLICT (workspace_id, slug) DO UPDATE SET
            destination_url = EXCLUDED.destination_url,
            active = true,
            channel_source = EXCLUDED.channel_source,
            action_id = EXCLUDED.action_id
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(&slug)
    .bind(&plan.destination_url)
    .bind(action_id)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query(
        r#"
        INSERT INTO autopilot_actions (
            id, workspace_id, decision_id, context, action_kind,
            subject_kind, subject_id, idempotency_key, payload, status,
            action_class, approved_at, approved_by, available_at,
            finished_at, trace_id, causation_id
        ) VALUES (
            $1,$2,$3,'fan_lifecycle','latarnik.mission.offer',
            'fan',$4,$5,$6,'succeeded','first_party_reversible',
            $7,'policy:latarnik_mission',$7,$7,$8,$9
        )
        "#,
    )
    .bind(action_id)
    .bind(workspace_id)
    .bind(decision_id)
    .bind(fan_id)
    .bind(&idempotency_key)
    .bind(&payload_json)
    .bind(now)
    .bind(action_trace.trace_id().into_uuid())
    .bind(action_trace.causation_id().map(|id| id.into_uuid()))
    .execute(&mut *tx)
    .await?;

    let mission_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO latarnik_missions (
            workspace_id, role_id, kind, event_id, content_source_id,
            prompt, share_text, action_id, smart_link_id, offered_at, expires_at
        ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)
        ON CONFLICT DO NOTHING
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(role_id)
    .bind(plan.kind.as_str())
    .bind(plan.event_id)
    .bind(plan.content_source_id)
    .bind(&plan.prompt)
    .bind(&share_text)
    .bind(action_id)
    .bind(smart_link_id)
    .bind(now)
    .bind(now + MISSION_LIFETIME)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(mission_id) = mission_id else {
        tx.rollback().await?;
        return Ok(None);
    };

    sqlx::query(
        "INSERT INTO autopilot_action_attempts
             (workspace_id, action_id, attempt_number, outcome, occurred_at)
         VALUES ($1,$2,1,'succeeded',$3)",
    )
    .bind(workspace_id)
    .bind(action_id)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(Some(mission_id))
}

/// Closes what has run its course, in order: a mission whose person was
/// brought someone after they tapped is `completed` (the only completion
/// there is); an open one past its expiry is `expired`. Returns
/// `(completed, expired)`.
///
/// "Brought someone" is a *qualified* referral attribution to the Latarnik's
/// own code (a pending, rejected or reversed one is not a fan), qualified at or
/// after the tap and inside the mission's life plus a week for the friend to
/// act. A tap alone never completes anything.
///
/// # Errors
///
/// Propagates the database error.
pub async fn settle(
    pool: &PgPool,
    workspace_id: Uuid,
    now: OffsetDateTime,
) -> Result<(u64, u64), LatarnikError> {
    let completed = sqlx::query(
        "UPDATE latarnik_missions m
            SET status = 'completed', completed_at = $2
          WHERE m.workspace_id = $1
            AND m.status = 'tapped'
            AND EXISTS (
                SELECT 1
                  FROM latarnik_roles lr
                  JOIN person_identities pi
                    ON pi.workspace_id = lr.workspace_id AND pi.person_id = lr.person_id
                   AND pi.kind = 'email' AND pi.platform IS NULL
                  JOIN fans fan
                    ON fan.workspace_id = pi.workspace_id AND fan.normalized_email = pi.value
                  JOIN referral_attributions ra
                    ON ra.workspace_id = fan.workspace_id AND ra.referrer_fan_id = fan.id
                 WHERE lr.workspace_id = m.workspace_id AND lr.id = m.role_id
                   AND ra.status = 'qualified'
                   AND ra.qualified_at >= m.tapped_at
                   AND ra.qualified_at <= m.expires_at + interval '7 days')",
    )
    .bind(workspace_id)
    .bind(now)
    .execute(pool)
    .await?
    .rows_affected();
    let expired = sqlx::query(
        "UPDATE latarnik_missions
            SET status = 'expired'
          WHERE workspace_id = $1
            AND status IN ('offered', 'tapped')
            AND expires_at <= $2",
    )
    .bind(workspace_id)
    .bind(now)
    .execute(pool)
    .await?
    .rows_affected();
    Ok((completed, expired))
}

/// A mission as the Latarnik sees it in their own session.
#[derive(Debug, FromRow, serde::Serialize)]
pub struct MyMission {
    pub id: Uuid,
    pub kind: String,
    pub prompt: String,
    pub share_text: String,
    pub status: String,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

const MY_FAN: &str = "
    SELECT fan.workspace_id, fan.id AS fan_id, fan.normalized_email
    FROM fan_sessions session
    JOIN fans fan
      ON fan.workspace_id = session.workspace_id AND fan.id = session.fan_id
    WHERE session.workspace_id = $1
      AND session.session_token_hash = digest($2, 'sha256')
      AND session.revoked_at IS NULL
      AND session.expires_at > now()
      AND fan.status = 'active'
      AND fan.deleted_at IS NULL";

/// The open mission for the signed-in fan, if any. `None` for no session and
/// for a fan with nothing open alike — the surface shows nothing either way.
///
/// # Errors
///
/// Propagates the database error.
pub async fn my_open_mission(
    pool: &PgPool,
    workspace_id: Uuid,
    session_token: &str,
    now: OffsetDateTime,
) -> Result<Option<MyMission>, LatarnikError> {
    Ok(sqlx::query_as::<_, MyMission>(&format!(
        "WITH me AS ({MY_FAN})
         SELECT m.id, m.kind, m.prompt, m.share_text, m.status, m.expires_at
         FROM me
         JOIN person_identities pi
           ON pi.workspace_id = me.workspace_id AND pi.kind = 'email'
          AND pi.platform IS NULL AND pi.value = me.normalized_email
         JOIN latarnik_roles lr
           ON lr.workspace_id = pi.workspace_id AND lr.person_id = pi.person_id
          AND lr.status = 'active'
         JOIN latarnik_missions m
           ON m.workspace_id = lr.workspace_id AND m.role_id = lr.id
          AND m.status IN ('offered', 'tapped') AND m.expires_at > $3
         LIMIT 1"
    ))
    .bind(workspace_id)
    .bind(session_token)
    .bind(now)
    .fetch_optional(pool)
    .await?)
}

/// What the person did with the mission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MissionAnswer {
    /// They tapped send: the share sheet opened. Recorded, never rewarded.
    Tap,
    /// Not now; the mission is closed and the cooldown runs from its offer.
    Dismiss,
}

/// Records the person's answer to their open mission. Returns `false` when
/// there was nothing it could apply to (no session, no such mission, not theirs,
/// or already closed) — one outcome for all of them, so the call cannot be used
/// to probe for other people's missions.
///
/// # Errors
///
/// Propagates the database error.
pub async fn answer_my_mission(
    pool: &PgPool,
    workspace_id: Uuid,
    session_token: &str,
    mission_id: Uuid,
    answer: MissionAnswer,
    now: OffsetDateTime,
) -> Result<bool, LatarnikError> {
    let (set, from): (&str, &str) = match answer {
        MissionAnswer::Tap => (
            "status = 'tapped', tapped_at = COALESCE(m.tapped_at, $4)",
            "m.status IN ('offered', 'tapped')",
        ),
        MissionAnswer::Dismiss => ("status = 'dismissed'", "m.status IN ('offered', 'tapped')"),
    };
    let changed = sqlx::query(&format!(
        "WITH me AS ({MY_FAN})
         UPDATE latarnik_missions m
            SET {set}
           FROM me, person_identities pi, latarnik_roles lr
          WHERE m.workspace_id = $1 AND m.id = $3 AND {from}
            AND m.expires_at > $4
            AND pi.workspace_id = me.workspace_id AND pi.kind = 'email'
            AND pi.platform IS NULL AND pi.value = me.normalized_email
            AND lr.workspace_id = pi.workspace_id AND lr.person_id = pi.person_id
            AND lr.id = m.role_id AND lr.status = 'active'"
    ))
    .bind(workspace_id)
    .bind(session_token)
    .bind(mission_id)
    .bind(now)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(changed > 0)
}
