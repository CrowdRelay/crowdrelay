//! The curator DM queue: published admin handles for channels nobody can
//! post to, listed per video with the message the operator would send.
//!
//! A handle candidate is deliberately not an outreach target — targets are
//! keyed on a contact address and a handle has none. Sending stays manual:
//! the queue drafts the words, the operator sends the DM, and
//! `mark_curator_dm_sent` writes the `outreach_interactions` row that takes
//! the candidate out of the queue for that video. The row links through the
//! nullable `candidate_id` migration 0380 added; `target_id` stays NULL.
//!
//! The draft is a fixed template in the tenant's plain voice — no LLM, no
//! hype — because the words a stranger reads should be the band's and the
//! same every time the situation is the same.

use crowdrelay_application::ports::{IdempotencyKey, RepositoryError, RequestId};
use crowdrelay_domain::WorkspaceId;
use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::database::{SqlxErrorClass, classify_sqlx_error};

fn map_sqlx(error: sqlx::Error) -> RepositoryError {
    match classify_sqlx_error(&error) {
        SqlxErrorClass::NotFound => RepositoryError::NotFound,
        SqlxErrorClass::Conflict => RepositoryError::Conflict,
        SqlxErrorClass::Unavailable => RepositoryError::Unavailable,
        SqlxErrorClass::Unexpected => RepositoryError::Unexpected,
    }
}

/// The source_key every curator-DM interaction carries for a video, so a
/// queue row can tell "already sent for this video" from "sent for another".
pub fn curator_source_key(source_id: Uuid) -> String {
    format!("manual:curator:{source_id}")
}

/// The words the operator sends. Deterministic on purpose: every curator
/// gets the same plainly-written ask with this video filled in.
#[must_use]
pub fn curator_dm(band: &str, channel: &str, title: &str, url: &str) -> String {
    format!(
        "Hi — we're {band}. We just put out \"{title}\": {url} — if it fits \
         {channel}, we'd be glad if you shared it. Thanks either way."
    )
}

#[derive(Clone, Debug, Serialize)]
pub struct CuratorQueueItem {
    pub candidate_id: Uuid,
    pub display_name: String,
    /// The published route — the `@handle` the DM goes to.
    pub handle: String,
    pub follower_count: Option<i32>,
    pub evidence: Option<String>,
    /// The place the handle was read from.
    pub source_reference: String,
    pub fit_basis_points: i32,
    /// Filled per video: the template with this video's title and URL.
    pub draft_dm: String,
    /// When the operator marked this candidate sent for this video — NULL
    /// while the row still waits in the queue.
    #[serde(with = "time::serde::rfc3339::option")]
    pub sent_at: Option<OffsetDateTime>,
}

/// What "mark sent" did. `replayed` distinguishes a duplicate request from a
/// recorded one without making the caller diff the row.
#[derive(Clone, Debug, Serialize)]
pub struct CuratorDmSent {
    pub candidate_id: Uuid,
    pub source_id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub sent_at: OffsetDateTime,
    pub replayed: bool,
}

struct VideoRow {
    title: String,
    url: Option<String>,
}

async fn load_video(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    source_id: Uuid,
) -> Result<Option<VideoRow>, RepositoryError> {
    sqlx::query_as::<_, (String, Option<String>)>(
        r#"
        SELECT title, metadata ->> 'url'
        FROM content_sources
        WHERE workspace_id = $1 AND id = $2 AND source_kind = 'video' AND active
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(source_id)
    .fetch_optional(pool)
    .await
    .map(|row| row.map(|(title, url)| VideoRow { title, url }))
    .map_err(map_sqlx)
}

/// `GET …/curator-queue`: admitted handle candidates ordered by audience
/// size, each carrying its drafted DM and whether it was already sent for
/// this video. `None` when `source_id` is not an active video of the
/// workspace — the handler maps that to 404.
pub async fn list_curator_queue(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    source_id: Uuid,
) -> Result<Option<Vec<CuratorQueueItem>>, RepositoryError> {
    let Some(video) = load_video(pool, workspace_id, source_id).await? else {
        return Ok(None);
    };
    let band = sqlx::query_scalar::<_, String>("SELECT name FROM workspaces WHERE id = $1")
        .bind(workspace_id.into_uuid())
        .fetch_one(pool)
        .await
        .map_err(map_sqlx)?;

    let rows = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            String,
            Option<i32>,
            Option<String>,
            String,
            i32,
            Option<OffsetDateTime>,
        ),
    >(
        r#"
        SELECT c.id, c.display_name, c.route_value, c.follower_count,
               c.evidence, c.source_reference, c.fit_basis_points,
               sent.occurred_at
        FROM outreach_candidates c
        LEFT JOIN outreach_interactions sent
          ON sent.workspace_id = c.workspace_id
         AND sent.candidate_id = c.id
         AND sent.source_key = $2
        WHERE c.workspace_id = $1
          AND c.status = 'admitted'
          AND c.route_kind = 'handle'
        ORDER BY c.follower_count DESC NULLS LAST, c.id
        LIMIT 200
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(curator_source_key(source_id))
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    let url = video.url.unwrap_or_default();
    Ok(Some(
        rows.iter()
            .map(
                |(id, name, handle, followers, evidence, reference, fit, sent_at)| {
                    CuratorQueueItem {
                        candidate_id: *id,
                        display_name: name.clone(),
                        handle: handle.clone(),
                        follower_count: *followers,
                        evidence: evidence.clone(),
                        source_reference: reference.clone(),
                        fit_basis_points: *fit,
                        draft_dm: curator_dm(&band, name, &video.title, &url),
                        sent_at: *sent_at,
                    }
                },
            )
            .collect(),
    ))
}

/// `POST …/curator-queue/{candidate_id}/sent`: the operator says the DM went
/// out, so the candidate gets its outbound interaction for this video.
///
/// The candidate must still be `admitted` — recording a send against a
/// refused or already-promoted row would let the ledger say contact happened
/// with a party screening rejected.
pub async fn mark_curator_dm_sent(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    source_id: Uuid,
    candidate_id: Uuid,
    note: Option<&str>,
    idempotency_key: &IdempotencyKey,
    request_id: Option<&RequestId>,
) -> Result<CuratorDmSent, RepositoryError> {
    if load_video(pool, workspace_id, source_id).await?.is_none() {
        return Err(RepositoryError::NotFound);
    }
    let mut transaction = pool.begin().await.map_err(map_sqlx)?;
    let operation_id = Uuid::now_v7();
    let details = serde_json::json!({
        "candidate_id": candidate_id,
        "source_id": source_id,
        "note": note,
    });
    if let Some(_existing) = crate::autopilot::operator_actions::insert_operator_action(
        &mut transaction,
        workspace_id,
        operation_id,
        "record_curator_dm_sent",
        "outreach_candidate",
        candidate_id,
        "operator",
        idempotency_key,
        request_id,
        &details,
    )
    .await?
    {
        let sent_at = sqlx::query_scalar::<_, OffsetDateTime>(
            r#"
            SELECT occurred_at FROM outreach_interactions
            WHERE workspace_id = $1 AND candidate_id = $2 AND source_key = $3
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(candidate_id)
        .bind(curator_source_key(source_id))
        .fetch_one(&mut *transaction)
        .await
        .map_err(map_sqlx)?;
        transaction.commit().await.map_err(map_sqlx)?;
        return Ok(CuratorDmSent {
            candidate_id,
            source_id,
            sent_at,
            replayed: true,
        });
    }

    let admissible = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM outreach_candidates
            WHERE workspace_id = $1 AND id = $2
              AND status = 'admitted' AND route_kind = 'handle'
        )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(candidate_id)
    .fetch_one(&mut *transaction)
    .await
    .map_err(map_sqlx)?;
    if !admissible {
        transaction.rollback().await.map_err(map_sqlx)?;
        return Err(RepositoryError::Conflict);
    }

    let sent_at = OffsetDateTime::now_utc();
    sqlx::query(
        r#"
        INSERT INTO outreach_interactions (
            workspace_id, target_id, candidate_id, direction, phase,
            source_key, occurred_at, metadata
        ) VALUES ($1, NULL, $2, 'outbound', 'initial', $3, $4, $5)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(candidate_id)
    .bind(curator_source_key(source_id))
    .bind(sent_at)
    .bind(serde_json::json!({ "note": note }))
    .execute(&mut *transaction)
    .await
    .map_err(map_sqlx)?;

    transaction.commit().await.map_err(map_sqlx)?;
    Ok(CuratorDmSent {
        candidate_id,
        source_id,
        sent_at,
        replayed: false,
    })
}
