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
//! touches until a person has looked. Each fact is windowed (the oldest touch it
//! can see is 30 days), and `acknowledge` is how a person says "reviewed": only
//! touches made *after* the newest acknowledgement of that kind can breach again,
//! so the same fault recurring halts the lane at once. Nothing else clears it
//! early — not the machine, and not a flag the code can set for itself.

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

    /// The acknowledgement key (the condition key without its `scout.` prefix).
    #[must_use]
    pub const fn ack_key(self) -> &'static str {
        match self {
            Self::ContactedAfterNo => "contacted_suppressed",
            Self::OverRate => "over_rate",
            Self::InviteWithoutRoute => "invite_without_route",
            Self::UntrackedLink => "untracked_link",
        }
    }

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
              AND t.touched_at > COALESCE((SELECT max(ack.acknowledged_at) FROM scout_breach_acknowledgements ack WHERE ack.workspace_id = $1 AND ack.breach = 'contacted_suppressed'), '-infinity'::timestamptz)
          ),
          (
            (SELECT count(*) FROM fan_prospect_touches t
              WHERE t.workspace_id = $1
                AND t.touched_at > now() - interval '24 hours'
                AND t.touched_at > COALESCE((SELECT max(ack.acknowledged_at) FROM scout_breach_acknowledgements ack WHERE ack.workspace_id = $1 AND ack.breach = 'over_rate'), '-infinity'::timestamptz)) > $2
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
                AND b.touched_at > COALESCE((SELECT max(ack.acknowledged_at) FROM scout_breach_acknowledgements ack WHERE ack.workspace_id = $1 AND ack.breach = 'over_rate'), '-infinity'::timestamptz)
            )
          ),
          EXISTS (
            SELECT 1 FROM fan_prospect_touches t
            WHERE t.workspace_id = $1
              AND t.kind = 'invite'
              AND t.touched_at > now() - interval '30 days'
              AND t.touched_at > COALESCE((SELECT max(ack.acknowledged_at) FROM scout_breach_acknowledgements ack WHERE ack.workspace_id = $1 AND ack.breach = 'invite_without_route'), '-infinity'::timestamptz)
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
              AND t.touched_at > COALESCE((SELECT max(ack.acknowledged_at) FROM scout_breach_acknowledgements ack WHERE ack.workspace_id = $1 AND ack.breach = 'untracked_link'), '-infinity'::timestamptz)
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

/// Records a person's decision that this kind of breach has been reviewed, so the
/// touches made so far no longer halt the lane. A note is required: it is the
/// reason resuming is safe, kept beside the decision.
///
/// # Errors
///
/// Propagates the database error. An empty note is refused by the schema.
pub async fn acknowledge(
    pool: &PgPool,
    workspace_id: Uuid,
    breach: Breach,
    acknowledged_by: &str,
    note: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO scout_breach_acknowledgements
             (workspace_id, breach, acknowledged_by, note)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(workspace_id)
    .bind(breach.ack_key())
    .bind(acknowledged_by)
    .bind(note)
    .execute(pool)
    .await?;
    Ok(())
}
