// Choosing the journalist a press pitch is addressed to.
//
// `include!`d into `agent_outcomes.rs` the way the fixture modules elsewhere
// are, so it shares that module's scope and imports. Split out so the decision
// is findable by name, and so the parent stays inside the source-size ratchet.

/// A press contact a pitch can actually be sent to.
#[derive(Debug, sqlx::FromRow)]
struct PressRecipient {
    id: Uuid,
    display_name: String,
    contact_email: String,
}

/// Picks the press target a pitch should go to.
///
/// Promoted targets first — an operator has looked at those and kept them —
/// then proposed ones, so the loop is not blocked waiting for promotion. Least
/// recently pitched first, so the same journalist is not contacted twice while
/// others have never been approached. `contact_email IS NOT NULL` is the whole
/// point: a press target without an address is a lead, not a recipient.
async fn press_recipient(
    pool: &PgPool,
    workspace_id: WorkspaceId,
) -> Result<Option<PressRecipient>, AgentOutcomeError> {
    let recipient = sqlx::query_as::<_, PressRecipient>(
        r#"
        SELECT target.id, target.display_name, target.contact_email
        FROM agent_outreach_targets AS target
        WHERE target.workspace_id = $1
          AND target.target_kind = 'press'
          AND target.status IN ('promoted', 'proposed')
          AND target.contact_email IS NOT NULL
          AND btrim(target.contact_email) <> ''
        ORDER BY
            CASE target.status WHEN 'promoted' THEN 0 ELSE 1 END,
            (
                SELECT max(a.created_at)
                FROM viryaos_autopilot_actions a
                WHERE a.workspace_id = target.workspace_id
                  AND a.action_kind = 'agent.content.request'
                  AND (a.payload->>'recipient_target_id')::uuid = target.id
            ) ASC NULLS FIRST,
            target.created_at ASC
        LIMIT 1
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_optional(pool)
    .await?;
    Ok(recipient)
}
