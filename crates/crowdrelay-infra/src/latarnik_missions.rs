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

use crowdrelay_domain::latarnik_mission::{
    Language, MISSION_LIFETIME, MissionContext, MissionPlan, ReleaseFact, SHOW_HORIZON, ShowFact,
};
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
                (SELECT max(m.offered_at) FROM latarnik_missions m
                  WHERE m.workspace_id = lr.workspace_id AND m.role_id = lr.id) AS last_offered_at,
                EXISTS (SELECT 1 FROM latarnik_missions m
                         WHERE m.workspace_id = lr.workspace_id AND m.role_id = lr.id
                           AND m.status IN ('offered', 'tapped')) AS has_open_mission,
                (SELECT rc.code FROM referral_codes rc
                  WHERE rc.workspace_id = fan.workspace_id AND rc.fan_id = fan.id AND rc.active
                  ORDER BY rc.created_at, rc.id LIMIT 1) AS referral_code
         FROM latarnik_roles lr
         JOIN person_identities pi
           ON pi.workspace_id = lr.workspace_id AND pi.person_id = lr.person_id
          AND pi.kind = 'email' AND pi.platform IS NULL
         JOIN fans fan
           ON fan.workspace_id = pi.workspace_id AND fan.normalized_email = pi.value
          AND fan.status = 'active' AND fan.deleted_at IS NULL
          AND fan.merged_into_fan_id IS NULL
         WHERE lr.workspace_id = $1
           AND lr.status = 'active'
           AND lr.capabilities ? 'referral_link'
         ORDER BY lr.activated_at, lr.id
         LIMIT $2",
    )
    .bind(workspace_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    // Shows and releases are the same for every Latarnik; their *city* is not.
    let shows = sqlx::query_as::<_, (Uuid, String, Option<Uuid>, Option<String>, OffsetDateTime)>(
        "SELECT e.id, e.title, e.city_id, c.name, e.starts_at
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
                language: Language::from_locale(row.locale.as_deref()),
                referral_url,
                shows: shows
                    .iter()
                    .map(|(event_id, title, city_id, city, starts_at)| ShowFact {
                        event_id: *event_id,
                        title: title.clone(),
                        city: city.clone(),
                        starts_on: starts_at.date(),
                        starts_at: *starts_at,
                        in_their_city: city_id.is_some_and(|id| cities.contains(&id)),
                    })
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
    plan: &MissionPlan,
    now: OffsetDateTime,
) -> Result<Option<Uuid>, LatarnikError> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO latarnik_missions
             (workspace_id, role_id, kind, event_id, content_source_id, prompt, share_text,
              offered_at, expires_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
         ON CONFLICT DO NOTHING
         RETURNING id",
    )
    .bind(workspace_id)
    .bind(role_id)
    .bind(plan.kind.as_str())
    .bind(plan.event_id)
    .bind(plan.content_source_id)
    .bind(&plan.prompt)
    .bind(&plan.share_text)
    .bind(now)
    .bind(now + MISSION_LIFETIME)
    .fetch_optional(pool)
    .await?)
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
