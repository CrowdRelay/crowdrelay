//! The roster archive (5.14, §4h-7.3): the shared counterparty record the
//! label's acts built without knowing it.
//!
//! `place_counterparty_marks` is workspace-scoped by construction — each
//! mark is one act's private "our show named this person". What a label
//! can read across its own roster is the recurrence: the same promoter
//! named by three acts is a relationship the label already holds three
//! times over, and the archive is where that density surfaces. One read
//! spans every member's marks — further back than any single act's own
//! history — while marks from outside the organisation never enter the
//! response: the boundary is `workspaces.organization_id`, same as every
//! roster-plan read.
//!
//! There is no decision here to keep in the domain: the archive is a
//! measured record, ordered by recurrence then recency, assembled from the
//! rows the event trigger maintains.

use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::portfolio::PortfolioError;

/// One act's share of a counterparty's record — which member met them, and
/// how often.
#[derive(Debug, Serialize)]
pub struct ActCounterpartyShare {
    pub workspace_id: Uuid,
    pub act_name: String,
    /// How many of the act's published or completed shows named this
    /// counterparty.
    pub shows: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub last_seen: OffsetDateTime,
}

/// One counterparty as the roster met them: the recurrence is the record.
#[derive(Debug, Serialize)]
pub struct CounterpartyRecord {
    pub counterparty_id: Uuid,
    /// The address the marks normalized to — the identity itself. The
    /// organisation's own acts recorded it; this read is theirs.
    pub email: String,
    /// The name the most recent mark contributed — `None` is "never
    /// recorded a name", not a guess.
    pub display_name: Option<String>,
    /// How many distinct member acts have named this person — the
    /// recurrence the archive exists to surface. `1` is an act's private
    /// contact; `3` is a roster relationship.
    pub shared_by_acts: i64,
    /// Total member marks — shows this counterparty was named on across
    /// the roster.
    pub shows: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub first_seen: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub last_seen: OffsetDateTime,
    pub acts: Vec<ActCounterpartyShare>,
}

/// The archive page: every counterparty the organisation's acts have
/// named, most-shared first.
#[derive(Debug, Serialize)]
pub struct CounterpartyArchive {
    pub organization_id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
    /// Member workspaces of the organisation — the archive's span.
    pub member_count: i64,
    pub counterparties: Vec<CounterpartyRecord>,
}

#[derive(Debug, sqlx::FromRow)]
struct CounterpartyHeadRow {
    counterparty_id: Uuid,
    email: String,
    shared_by_acts: i64,
    shows: i64,
    first_seen: OffsetDateTime,
    last_seen: OffsetDateTime,
    display_name: Option<String>,
}

#[derive(Debug, sqlx::FromRow)]
struct ShareRow {
    counterparty_id: Uuid,
    workspace_id: Uuid,
    act_name: String,
    shows: i64,
    last_seen: OffsetDateTime,
}

/// The archive for one organisation's roster.
///
/// # Errors
///
/// Propagates the database error as [`PortfolioError::Database`].
pub async fn counterparty_archive(
    pool: &PgPool,
    organization_id: Uuid,
    now: OffsetDateTime,
) -> Result<CounterpartyArchive, PortfolioError> {
    let member_count =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM workspaces WHERE organization_id = $1")
            .bind(organization_id)
            .fetch_one(pool)
            .await
            .map_err(PortfolioError::Database)?;

    // Heads: one row per counterparty any member marked, with the
    // recurrence and the span. The display name is the most recent mark's
    // contribution — DISTINCT ON keeps it the newest observed spelling.
    let heads = sqlx::query_as::<_, CounterpartyHeadRow>(
        r#"
        WITH member_marks AS (
            SELECT mark.counterparty_id, mark.workspace_id,
                   mark.contributed_name, mark.marked_at
            FROM place_counterparty_marks AS mark
            JOIN workspaces AS member
              ON member.id = mark.workspace_id
             AND member.organization_id = $1
        ), latest_names AS (
            SELECT DISTINCT ON (counterparty_id)
                   counterparty_id, contributed_name
            FROM member_marks
            ORDER BY counterparty_id, marked_at DESC
        )
        SELECT counterparty.id AS counterparty_id,
               counterparty.email_key AS email,
               count(DISTINCT member_marks.workspace_id) AS shared_by_acts,
               count(*) AS shows,
               min(member_marks.marked_at) AS first_seen,
               max(member_marks.marked_at) AS last_seen,
               latest_names.contributed_name AS display_name
        FROM place_counterparties AS counterparty
        JOIN member_marks ON member_marks.counterparty_id = counterparty.id
        LEFT JOIN latest_names ON latest_names.counterparty_id = counterparty.id
        GROUP BY counterparty.id, counterparty.email_key,
                 latest_names.contributed_name
        ORDER BY shared_by_acts DESC, last_seen DESC, counterparty.id
        "#,
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await
    .map_err(PortfolioError::Database)?;

    // Per-act shares for the same set — the breakdown the recurrence is
    // made of.
    let shares = sqlx::query_as::<_, ShareRow>(
        r#"
        SELECT mark.counterparty_id, mark.workspace_id,
               member.name AS act_name,
               count(*) AS shows,
               max(mark.marked_at) AS last_seen
        FROM place_counterparty_marks AS mark
        JOIN workspaces AS member
          ON member.id = mark.workspace_id
         AND member.organization_id = $1
        GROUP BY mark.counterparty_id, mark.workspace_id, member.name
        ORDER BY mark.counterparty_id, shows DESC, member.name
        "#,
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await
    .map_err(PortfolioError::Database)?;

    let mut shares_by_counterparty: std::collections::HashMap<Uuid, Vec<ActCounterpartyShare>> =
        std::collections::HashMap::new();
    for share in shares {
        shares_by_counterparty
            .entry(share.counterparty_id)
            .or_default()
            .push(ActCounterpartyShare {
                workspace_id: share.workspace_id,
                act_name: share.act_name,
                shows: share.shows,
                last_seen: share.last_seen,
            });
    }

    let counterparties = heads
        .into_iter()
        .map(|head| CounterpartyRecord {
            counterparty_id: head.counterparty_id,
            email: head.email,
            display_name: head.display_name,
            shared_by_acts: head.shared_by_acts,
            shows: head.shows,
            first_seen: head.first_seen,
            last_seen: head.last_seen,
            acts: shares_by_counterparty
                .remove(&head.counterparty_id)
                .unwrap_or_default(),
        })
        .collect();

    Ok(CounterpartyArchive {
        organization_id,
        generated_at: now,
        member_count,
        counterparties,
    })
}
