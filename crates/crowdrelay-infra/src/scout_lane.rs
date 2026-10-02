//! The FAN SCOUT lane's tripwires: the facts that mean the lane must stop.
//!
//! The lane speaks to people the band has never been introduced to, so it runs
//! autonomously only because its envelope is enforced. These are the four ways
//! the envelope can be seen to have been breached in the rows the lane writes
//! (`fan_prospect_touches`). Each is a plain read; each is **critical**: the
//! lane's senders ask [`halt`] before they speak, and the watchdog raises the
//! same facts as `scout.*` conditions, so what the operator is told and what
//! the lane does are one answer, not two that can drift.
//!
//! A halt is not a verdict on any person: it stops the lane from adding more
//! touches until the cause is gone. Each fact is windowed (the oldest touch it
//! can see is 30 days), so a breach that was investigated and fixed clears
//! itself rather than needing a flag to be reset by hand.

use sqlx::PgPool;
use uuid::Uuid;

/// Touches the lane may make in any rolling day. A ceiling, not a target: the
/// first weeks of an autonomous lane are meant to be slow.
pub const DAILY_TOUCH_CAP: i64 = 20;
/// Two touches to one person closer than this are two voices at once.
pub const ONE_VOICE_HOURS: i32 = 72;

/// What the lane's own rows show.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Breach {
    /// A touch was recorded after the person was refused or suppressed.
    ContactedAfterNo,
    /// More touches in a day than [`DAILY_TOUCH_CAP`], or two to one person
    /// inside [`ONE_VOICE_HOURS`].
    OverRate,
    /// An invitation to someone who never spoke to the band in a thread the
    /// band reads: no lawful route.
    InviteWithoutRoute,
    /// An invitation whose tracked link is no longer live: a person following it
    /// reaches nothing, and the click can never be attributed.
    UntrackedLink,
}

impl Breach {
    pub const ALL: [Self; 4] = [
        Self::ContactedAfterNo,
        Self::OverRate,
        Self::InviteWithoutRoute,
        Self::UntrackedLink,
    ];

    /// The watchdog condition key.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::ContactedAfterNo => "scout.contacted_suppressed",
            Self::OverRate => "scout.over_rate",
            Self::InviteWithoutRoute => "scout.invite_without_route",
            Self::UntrackedLink => "scout.untracked_link",
        }
    }
}

/// Every breach the lane's rows currently show.
///
/// # Errors
///
/// Propagates the database error. The caller decides what an unreadable lane
/// means; the senders treat it as a halt (see `halted`), never as "all clear".
pub async fn breaches(pool: &PgPool, workspace_id: Uuid) -> Result<Vec<Breach>, sqlx::Error> {
    let (contacted_after_no, over_rate, invite_without_route, untracked_link): (
        bool,
        bool,
        bool,
        bool,
    ) = sqlx::query_as(
        r#"
        SELECT
          EXISTS (
            SELECT 1 FROM fan_prospect_touches t
            JOIN fan_prospects p
              ON p.workspace_id = t.workspace_id AND p.id = t.prospect_id
            WHERE t.workspace_id = $1
              AND t.touched_at > now() - interval '30 days'
              AND p.status IN ('refused', 'suppressed')
              -- A refusal stops accepting writes, so its own timestamp is the
              -- moment of the no: a touch after it spoke to someone who said it.
              AND t.touched_at > p.updated_at
          ),
          (
            (SELECT count(*) FROM fan_prospect_touches t
              WHERE t.workspace_id = $1
                AND t.touched_at > now() - interval '24 hours') > $2
            OR EXISTS (
              SELECT 1 FROM fan_prospect_touches a
              JOIN fan_prospect_touches b
                ON b.workspace_id = a.workspace_id
               AND b.prospect_id = a.prospect_id
               AND b.id <> a.id
               AND b.touched_at > a.touched_at
               AND b.touched_at < a.touched_at + make_interval(hours => $3)
              WHERE a.workspace_id = $1
                AND b.touched_at > now() - interval '7 days'
            )
          ),
          EXISTS (
            SELECT 1 FROM fan_prospect_touches t
            WHERE t.workspace_id = $1
              AND t.kind = 'invite'
              AND t.touched_at > now() - interval '30 days'
              AND NOT EXISTS (
                SELECT 1 FROM fan_prospect_observations o
                WHERE o.workspace_id = t.workspace_id
                  AND o.prospect_id = t.prospect_id
                  AND o.source = 'own_comments'
              )
          ),
          EXISTS (
            SELECT 1 FROM fan_prospect_touches t
            JOIN smart_links l ON l.id = t.smart_link_id
            WHERE t.workspace_id = $1
              AND t.kind = 'invite'
              AND t.touched_at > now() - interval '30 days'
              AND NOT l.active
          )
        "#,
    )
    .bind(workspace_id)
    .bind(DAILY_TOUCH_CAP)
    .bind(ONE_VOICE_HOURS)
    .fetch_one(pool)
    .await?;
    Ok(Breach::ALL
        .into_iter()
        .zip([
            contacted_after_no,
            over_rate,
            invite_without_route,
            untracked_link,
        ])
        .filter_map(|(breach, present)| present.then_some(breach))
        .collect())
}

/// Why the lane may not speak right now, if it may not.
///
/// An unreadable lane is a halt: the senders must not take silence from the
/// database as permission.
pub async fn halted(pool: &PgPool, workspace_id: Uuid) -> Option<Vec<Breach>> {
    match breaches(pool, workspace_id).await {
        Ok(found) if found.is_empty() => None,
        Ok(found) => Some(found),
        Err(error) => {
            tracing::error!(%error, "scout lane tripwires unreadable; holding the lane");
            Some(Vec::new())
        }
    }
}
