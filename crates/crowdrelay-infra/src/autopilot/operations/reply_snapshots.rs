//! The unanswered-reply snapshot loader: one row per outreach conversation
//! still waiting on the band, newest reply wins, oldest waiting first.

use super::*;

#[derive(Debug, FromRow)]
struct UnansweredReplyRow {
    interaction_id: i64,
    target_id: Uuid,
    target_version: i64,
    target_name: String,
    target_kind: String,
    disposition: String,
    sheet_verdict: Option<String>,
    replied_at: OffsetDateTime,
}

/// One snapshot per conversation still waiting on the band — the newest
/// inbound reply on each target that no outbound touch has answered.
///
/// The DISTINCT ON keeps this honest about what a reply action is: an
/// answer to a *conversation*, not to a row. Two unanswered replies on the
/// same target collapse to the newest, so the action a person sees is the
/// one they are actually waiting on. `received` and `positive` are the
/// only dispositions that load — a recorded refusal does not get a
/// machine-written response, and `do_not_contact` is excluded twice over
/// (here, and on the target) because the line does not move for a reply.
///
/// The verdict rides along raw: `response_type` is what `promo:` rows
/// carry, `result` what `master:` rows carry. It picks the draft scaffold's
/// shape and lands on the action for audit — it is never quoted to the
/// recipient.
pub(in crate::autopilot) async fn load_unanswered_reply_snapshots(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    _now: OffsetDateTime,
) -> Result<Vec<UnansweredReplySnapshot>, RepositoryError> {
    let rows = sqlx::query_as::<_, UnansweredReplyRow>(
        r#"
        SELECT interaction_id, target_id, target_version, target_name,
               target_kind, disposition, sheet_verdict, replied_at
        FROM (
            SELECT DISTINCT ON (interaction.target_id)
                interaction.id AS interaction_id,
                target.id AS target_id,
                target.version AS target_version,
                target.display_name AS target_name,
                target.target_kind,
                interaction.disposition,
                COALESCE(
                    interaction.metadata->>'response_type',
                    interaction.metadata->>'result'
                ) AS sheet_verdict,
                interaction.occurred_at AS replied_at
            FROM outreach_interactions AS interaction
            JOIN outreach_targets AS target
              ON target.workspace_id = interaction.workspace_id
             AND target.id = interaction.target_id
            WHERE interaction.workspace_id = $1
              AND interaction.direction = 'inbound'
              AND interaction.phase = 'reply'
              AND interaction.disposition IN ('received','positive')
              AND target.active
              AND NOT target.do_not_contact
            ORDER BY interaction.target_id,
                     interaction.occurred_at DESC,
                     interaction.id DESC
        ) AS newest
        WHERE NOT EXISTS (
            SELECT 1
            FROM outreach_interactions AS answered
            WHERE answered.workspace_id = $1
              AND answered.target_id = newest.target_id
              AND answered.direction = 'outbound'
              -- Strictly later, matching the attention board and the
              -- briefing: the sheet import falls back to `replied_at =
              -- sent_at` when the row carries no reply date, and a
              -- same-timestamp outbound cannot be proven to answer it.
              AND answered.occurred_at > newest.replied_at
        )
        -- Oldest waiting first: a reply stale since July outranks one from
        -- yesterday, because the person has been waiting longest.
        ORDER BY replied_at
        LIMIT $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(MAX_SNAPSHOTS_PER_CONTEXT)
    .fetch_all(&repo.pool)
    .await
    .map_err(map_sqlx)?;

    rows.into_iter()
        .map(|row| {
            Ok(UnansweredReplySnapshot {
                interaction_id: row.interaction_id,
                target_id: OutreachTargetId::from_uuid(row.target_id),
                target_version: row.target_version,
                target_name: row.target_name,
                target_kind: OutreachTargetKind::parse(&row.target_kind)
                    .ok_or(RepositoryError::Unexpected)?,
                reply_disposition: parse_outreach_reply(&row.disposition)?,
                sheet_verdict: row.sheet_verdict,
                replied_at: row.replied_at,
            })
        })
        .collect()
}
