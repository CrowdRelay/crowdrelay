//! The label's quarterly page to one of its own acts (5.25).
//!
//! Labels lose acts because the act cannot see what the label did. §4f-1's
//! counterparty machinery pointed inward: this read composes the quarter's
//! record for one member act from rows that already exist — what the brain
//! dispatched for them, which rooms they played, where the new fans are,
//! and which catalogue rotations landed. Nothing is derived twice: the
//! counts are the same tables the act's own surfaces read, so the page and
//! the act's dashboard can never disagree.
//!
//! Admin rather than control-plane, like the rest of `roster-plan`: the
//! membership check is the boundary — an act outside the organisation gets
//! no page, and a workspace token never reaches this surface.

use serde::Serialize;
use sqlx::{FromRow, PgPool};
use time::{Date, Month, OffsetDateTime};
use uuid::Uuid;

use crowdrelay_application::RepositoryError;

use crate::database::{SqlxErrorClass, classify_sqlx_error};

/// The template key 1R.8's catalogue-rotation rung sends under — a rotation
/// the act received this quarter is one of these campaigns, not a count
/// inferred from "something with 'catalogue' in the name".
pub const CATALOGUE_ROTATION_TEMPLATE: &str = "release.catalogue_rotation.v1";

/// One dispatched-action line: the kind and its outcomes, counted.
#[derive(Clone, Debug, Serialize)]
pub struct ActReportActionLine {
    pub action_kind: String,
    pub status: String,
    pub count: u32,
}

/// One room the act played in the quarter — venue, city, date. `status` is
/// the event's own (`completed`, `published`); a cancelled night is not a
/// room played and is filtered out rather than listed as one. `venue` is
/// `null` when the event never named its room — `events.venue` is nullable,
/// and decoding it as a string failed the whole report for any act with one
/// such show on its bill.
#[derive(Clone, Debug, Serialize)]
pub struct ActReportShow {
    pub event_id: Uuid,
    pub venue: Option<String>,
    pub city: String,
    #[serde(with = "time::serde::rfc3339")]
    pub starts_at: OffsetDateTime,
    pub status: String,
}

/// Where the quarter's new fans said they are — one declared interest per
/// row. A fan can declare several cities and none; the by-city list is the
/// distribution of declared interest, and `gained_total` is the honest
/// whole, so the parts visibly need not sum to it.
#[derive(Clone, Debug, Serialize)]
pub struct ActReportFans {
    pub gained_total: u32,
    pub by_city: Vec<ActReportCityGain>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ActReportCityGain {
    pub city: String,
    pub count: u32,
}

/// One release-collision warning involving this act, from the roster's
/// release calendar — the label's answer to "is this act about to fight a
/// labelmate for the same week". The collision is org data, so it lives on
/// this admin surface and never on the act's own control-plane page.
#[derive(Clone, Debug, Serialize)]
pub struct ActReleaseCollision {
    /// The ISO Monday the colliding releases share — the frame the
    /// calendar argues in.
    pub week_start: Date,
    /// This act's release in the collision.
    pub release_id: Uuid,
    pub release_title: String,
    /// The labelmate sharing the week, and what of theirs collides.
    pub other_act: String,
    pub other_release_title: String,
    /// True when the calendar's standing order asks this act to move —
    /// false when the act keeps the week and the labelmate is asked.
    pub this_act_moves: bool,
    /// Count-only shared fans between the two workspaces — `Some(0)` is a
    /// counted zero (the overlap query measures every member pair), and a
    /// press clash only.
    pub shared_fans: Option<u32>,
    /// The calendar's own sentence on who keeps the week and why.
    pub reason: String,
}

/// The quarterly page for one member act.
#[derive(Clone, Debug, Serialize)]
pub struct RosterActReport {
    pub organization_id: Uuid,
    pub workspace_id: Uuid,
    pub act_name: String,
    /// Calendar-quarter-to-date: the first day of the current quarter
    /// through `generated_at`. Stated as dates so "quarter" is checkable
    /// rather than assumed.
    pub period_start: Date,
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
    /// Every action the brain dispatched for this act in the period, by
    /// kind and outcome — the "what the label did" section.
    pub actions: Vec<ActReportActionLine>,
    /// The rooms the act played — published or completed events the act is
    /// on the bill of, in the period.
    pub shows: Vec<ActReportShow>,
    pub fans: ActReportFans,
    /// Catalogue-rotation campaigns that actually sent for the act in the
    /// period (`status = 'completed'` — a draft or a cancelled send is not a
    /// rotation that landed). The crossbill the label owes under §4h-7.4,
    /// counted where it lands.
    pub rotations_landed: u32,
    /// Every release collision this act is a party to in the lookahead
    /// window — the calendar's own verdicts filtered to this act, so the
    /// report and the calendar can never disagree about who was asked to
    /// move. Empty is a clear run, not an unmeasured one.
    pub release_collisions: Vec<ActReleaseCollision>,
}

/// The first day of the calendar quarter containing `now`. Quarter months
/// are 1/4/7/10 — arithmetic on `Month`, not a lookup table.
fn quarter_start(now: OffsetDateTime) -> Date {
    let month = u8::from(now.month());
    let first = month - (month - 1) % 3;
    // `first` is always one of 1/4/7/10 and day 1 always exists — the
    // `unwrap_or` arms are unreachable by construction, not a failure mode.
    Date::from_calendar_date(
        now.year(),
        Month::try_from(first).unwrap_or(Month::January),
        1,
    )
    .unwrap_or(now.date())
}

#[derive(Debug, FromRow)]
struct ActionRow {
    action_kind: String,
    status: String,
    count: i64,
}

#[derive(Debug, FromRow)]
struct ShowRow {
    event_id: Uuid,
    venue: Option<String>,
    city: String,
    starts_at: OffsetDateTime,
    status: String,
}

/// Composes one member act's quarterly page.
///
/// Returns `Ok(None)` when the workspace is not a member of the
/// organisation — a page about somebody else's act is not a report this
/// surface produces, and the handler renders that as the absent answer.
///
/// # Errors
///
/// Propagates the database error, classified.
pub async fn roster_act_report(
    pool: &PgPool,
    organization_id: Uuid,
    workspace_id: Uuid,
    now: OffsetDateTime,
) -> Result<Option<RosterActReport>, RepositoryError> {
    let member = sqlx::query_as::<_, (String,)>(
        "SELECT name FROM workspaces WHERE id = $1 AND organization_id = $2",
    )
    .bind(workspace_id)
    .bind(organization_id)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx)?;
    let Some((act_name,)) = member else {
        return Ok(None);
    };

    let start = quarter_start(now);
    let period_start = OffsetDateTime::new_utc(start, time::Time::MIDNIGHT);

    let actions = sqlx::query_as::<_, ActionRow>(
        r#"
        SELECT action_kind, status, COUNT(*) AS count
        FROM autopilot_actions
        WHERE workspace_id = $1 AND created_at >= $2
        GROUP BY action_kind, status
        ORDER BY count DESC, action_kind, status
        "#,
    )
    .bind(workspace_id)
    .bind(period_start)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?
    .into_iter()
    .map(|row| ActReportActionLine {
        action_kind: row.action_kind,
        status: row.status,
        count: u32::try_from(row.count.max(0)).unwrap_or(u32::MAX),
    })
    .collect();

    let shows = sqlx::query_as::<_, ShowRow>(
        r#"
        SELECT event.id AS event_id, event.venue, city.name AS city,
               event.starts_at, event.status
        FROM events AS event
        JOIN event_acts AS act ON act.event_id = event.id
        JOIN cities AS city ON city.id = event.city_id
        WHERE act.act_workspace_id = $1
          AND event.starts_at >= $2
          AND event.starts_at <= $3
          AND event.status IN ('published', 'completed')
        ORDER BY event.starts_at, event.id
        LIMIT 200
        "#,
    )
    .bind(workspace_id)
    .bind(period_start)
    .bind(now)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?
    .into_iter()
    .map(|row| ActReportShow {
        event_id: row.event_id,
        venue: row.venue,
        city: row.city,
        starts_at: row.starts_at,
        status: row.status,
    })
    .collect();

    let gained_total = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)
        FROM fans
        WHERE workspace_id = $1
          AND created_at >= $2
          AND deleted_at IS NULL
        "#,
    )
    .bind(workspace_id)
    .bind(period_start)
    .fetch_one(pool)
    .await
    .map_err(map_sqlx)?;

    let by_city = sqlx::query_as::<_, (String, i64)>(
        r#"
        SELECT city.name, COUNT(*) AS count
        FROM fan_city_interests AS interest
        JOIN cities AS city ON city.id = interest.city_id
        JOIN fans AS fan ON fan.id = interest.fan_id
        WHERE interest.workspace_id = $1
          AND fan.created_at >= $2
          AND fan.deleted_at IS NULL
        GROUP BY city.name
        ORDER BY count DESC, city.name
        LIMIT 50
        "#,
    )
    .bind(workspace_id)
    .bind(period_start)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?
    .into_iter()
    .map(|(city, count)| ActReportCityGain {
        city,
        count: u32::try_from(count.max(0)).unwrap_or(u32::MAX),
    })
    .collect();

    let rotations_landed = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)
        FROM communication_campaigns
        WHERE workspace_id = $1
          AND template_key = $2
          AND created_at >= $3
          AND status = 'completed'
        "#,
    )
    .bind(workspace_id)
    .bind(CATALOGUE_ROTATION_TEMPLATE)
    .bind(period_start)
    .fetch_one(pool)
    .await
    .map_err(map_sqlx)?;

    // The act's release collisions, filtered from the whole organisation's
    // calendar — one definition of "collision" and of shared fans, so this
    // page cannot grow a second rule the calendar does not know about.
    let calendar =
        crate::roster_release_calendar::roster_release_calendar(pool, organization_id, now).await?;
    let release_collisions = calendar
        .collisions
        .iter()
        .filter_map(|collision| {
            let this_act_moves = if collision.moves_workspace_id.into_uuid() == workspace_id {
                true
            } else if collision.stays_workspace_id.into_uuid() == workspace_id {
                false
            } else {
                return None;
            };
            let (own_id, other_id) = if this_act_moves {
                (collision.moves_workspace_id, collision.stays_workspace_id)
            } else {
                (collision.stays_workspace_id, collision.moves_workspace_id)
            };
            let own = collision
                .releases
                .iter()
                .find(|release| release.workspace_id == own_id);
            let other = collision
                .releases
                .iter()
                .find(|release| release.workspace_id == other_id);
            let (Some(own), Some(other)) = (own, other) else {
                return None;
            };
            Some(ActReleaseCollision {
                week_start: collision.week_start,
                release_id: own.release_id,
                release_title: own.title.clone(),
                other_act: if this_act_moves {
                    collision.stays_act.clone()
                } else {
                    collision.moves_act.clone()
                },
                other_release_title: other.title.clone(),
                this_act_moves,
                shared_fans: collision.shared_fans,
                reason: collision.reason.clone(),
            })
        })
        .collect();

    Ok(Some(RosterActReport {
        organization_id,
        workspace_id,
        act_name,
        period_start: start,
        generated_at: now,
        actions,
        shows,
        fans: ActReportFans {
            gained_total: u32::try_from(gained_total.max(0)).unwrap_or(u32::MAX),
            by_city,
        },
        rotations_landed: u32::try_from(rotations_landed.max(0)).unwrap_or(u32::MAX),
        release_collisions,
    }))
}

/// Narrows a database failure to the repository's error vocabulary — the same
/// mapping every sibling module applies, so the read fails the way the rest
/// of the surface fails.
fn map_sqlx(error: sqlx::Error) -> RepositoryError {
    match classify_sqlx_error(&error) {
        SqlxErrorClass::NotFound => RepositoryError::NotFound,
        SqlxErrorClass::Conflict => RepositoryError::Conflict,
        SqlxErrorClass::Unavailable => RepositoryError::Unavailable,
        SqlxErrorClass::Unexpected => RepositoryError::Unexpected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    /// Quarter arithmetic pinned at the boundaries — the first day of Q1,
    /// a quarter's last month, and a year that does not slide backward.
    #[test]
    fn the_quarter_starts_on_its_first_months_first_day() {
        assert_eq!(
            quarter_start(datetime!(2026-09-18 12:00 UTC)),
            Date::from_calendar_date(2026, Month::July, 1).expect("valid date")
        );
        assert_eq!(
            quarter_start(datetime!(2026-01-05 12:00 UTC)),
            Date::from_calendar_date(2026, Month::January, 1).expect("valid date")
        );
        assert_eq!(
            quarter_start(datetime!(2026-12-31 23:59 UTC)),
            Date::from_calendar_date(2026, Month::October, 1).expect("valid date")
        );
    }
}
