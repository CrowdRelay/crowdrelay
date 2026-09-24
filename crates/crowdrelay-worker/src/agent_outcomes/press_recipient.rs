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

/// The contact a press pitch is addressed to: the one the model wrote it for.
///
/// The pitch names its addressee in `target_refs`, an id from the operator's
/// `outreach_targets` registry — the list the agents service shows the model.
/// This used to ignore that and pick the least recently pitched promoted row
/// from `agent_outreach_targets`, a different table: a pitch written for one
/// radio show, with that show's name and angle in it, went on the approval
/// queue addressed to whichever contact was next in line (found 2026-09-24,
/// after 380 registry contacts were imported and the drafts became specific).
///
/// Exactly one reference, or no recipient. A pitch naming several targets was
/// written to nobody in particular, and choosing one of them is choosing who
/// reads text addressed to someone else. The row must still be active,
/// accept outreach, not be do-not-contact, carry an address, and not be inside
/// the contact governor's cooldown for that address — the same guards the
/// executor would otherwise meet after a person had already approved the
/// wrong letter. Anything else is `None`, and the caller refuses the outcome
/// rather than substituting a different recipient.
async fn press_recipient(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    draft: Option<&Value>,
) -> Result<Option<PressRecipient>, AgentOutcomeError> {
    let Some(target_id) = single_target_ref(draft) else {
        return Ok(None);
    };
    let recipient = sqlx::query_as::<_, PressRecipient>(
        r#"
        SELECT target.id, target.display_name, target.contact_email
        FROM outreach_targets AS target
        WHERE target.workspace_id = $1
          AND target.id = $2
          AND target.active
          AND target.accepts_outreach
          AND NOT target.do_not_contact
          AND target.contact_email IS NOT NULL
          AND btrim(target.contact_email) <> ''
          AND NOT EXISTS (
              SELECT 1 FROM contact_governor AS governor
              WHERE governor.workspace_id = target.workspace_id
                AND governor.normalized_contact = lower(btrim(target.contact_email))
                AND (governor.do_not_contact OR governor.next_contact_after > now())
          )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(target_id)
    .fetch_optional(pool)
    .await?;
    Ok(recipient)
}

/// The one target id a pitch names, or `None` when it names none, several,
/// or something that is not an id.
fn single_target_ref(draft: Option<&Value>) -> Option<Uuid> {
    let refs: std::collections::BTreeSet<Uuid> = draft?
        .get("target_refs")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .filter_map(|s| Uuid::parse_str(s.trim()).ok())
        .collect();
    if refs.len() == 1 { refs.into_iter().next() } else { None }
}
