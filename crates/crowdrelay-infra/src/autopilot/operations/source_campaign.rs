//! The drop surge's email leg: a fan email anchored on a content source.
//!
//! Sibling of the event-bound audience campaign — a video has no event, no
//! city, and no lifecycle phase, so it runs the same segment + campaign +
//! `communication.campaign_due` machinery with a source anchor instead, and
//! addresses every consented fan: a drop is news for the whole list.

use super::*;

/// The payload fields a source-anchored fan campaign needs at send time —
/// sibling of `AudienceCampaignOrder` with a content source in place of the
/// event.
pub(in crate::autopilot) struct SourceCampaignOrder<'a> {
    pub source_id: ContentSourceId,
    pub template_key: &'a str,
    pub draft: &'a crowdrelay_domain::campaign_lifecycle::EventCampaignCopy,
}

/// Executes the source-anchored campaign. Same send contract as
/// `execute_audience_campaign`; the copy is the approved text on the
/// action, sent verbatim — a payload that lost it is refused rather than
/// re-composed at send time.
pub(in crate::autopilot) async fn execute_source_campaign(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    order: SourceCampaignOrder<'_>,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let SourceCampaignOrder {
        source_id,
        template_key,
        draft,
    } = order;
    // Same refusal as the event campaign: the copy is approved text, and a
    // row that lost it sends nothing rather than inventing words.
    if draft.subject.trim().is_empty() || draft.body.trim().is_empty() {
        return Err(RepositoryError::ConflictBecause(
            "source campaign refused: this action carries no copy — nothing may write one on the tenant's behalf at send time",
        ));
    }
    let feature = sqlx::query_scalar::<_, bool>(
        "SELECT COALESCE((SELECT enabled FROM ecosystem_feature_flags \
         WHERE workspace_id = $1 AND key = 'communication_campaigns_enabled'), false)",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    if !feature {
        return Err(RepositoryError::Conflict);
    }
    let source = sqlx::query_as::<_, (String, String)>(
        "SELECT source_key, title FROM content_sources WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind(source_id.into_uuid())
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::Conflict)?;
    // The `-mail` suffix keeps the campaign's slug out of the per-lane link
    // slug namespace (`drop-{key}-email` is the tracked link this email
    // carries) while still naming the source plainly in the ledger.
    let source_slug = crowdrelay_domain::content_supply::drop_surge_link_slug(&source.0, "mail")
        .unwrap_or_else(|| format!("drop-{}", source_id.into_uuid().simple()));
    let segment_slug = format!("crowdrelay-{source_slug}");
    let campaign_slug = segment_slug.clone();
    // Every fan who can lawfully hear it: active, marketing consent granted.
    let filter = json!({"statuses":["active"],"marketing_consent":true});
    let segment_id = sqlx::query_scalar::<_, Uuid>(
        r#"
      INSERT INTO audience_segments(workspace_id,slug,name,description,filter,active)
      VALUES($1,$2,$3,'CrowdRelay managed drop segment',$4,true)
      ON CONFLICT(workspace_id,slug) DO UPDATE SET filter=EXCLUDED.filter,active=true
      RETURNING id
    "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&segment_slug)
    .bind(format!("{} · drop", source.1.trim()))
    .bind(filter)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    let campaign = sqlx::query_as::<_, (Uuid, String, Option<OffsetDateTime>)>(
        r#"
        INSERT INTO communication_campaigns
            (workspace_id, segment_id, slug, name, channel, template_key, content)
        VALUES ($1, $2, $3, $4, 'email', $5,
                jsonb_build_object('source_id', $6::uuid, 'managed_by', 'crowdrelay',
                                   'subject', $7::text, 'body', $8::text))
        ON CONFLICT (workspace_id, slug) DO UPDATE SET
            template_key = communication_campaigns.template_key
        RETURNING id, status, scheduled_at
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(segment_id)
    .bind(&campaign_slug)
    .bind(format!("{} · drop", source.1.trim()))
    .bind(template_key)
    .bind(source_id.into_uuid())
    .bind(&draft.subject)
    .bind(&draft.body)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    if campaign.1 == "draft" {
        // The outbox row carries the action's trace spine, same as the
        // event campaign's; `send_evidence.source_id` is the provenance key
        // in the same vocabulary (`source-campaign:{slug}` where the
        // sibling writes `event-campaign:{slug}`).
        let outbox_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO outbox_events
                (workspace_id, event_type, event_version, payload, available_at,
                 trace_id, causation_id, action_id)
            SELECT $1, 'communication.campaign_due', 1,
                   jsonb_build_object(
                       'campaign_id', $2::uuid,
                       'campaign_slug', $3::text,
                       'channel', 'email',
                       'segment_id', $4::uuid,
                       'template_key', $5::text,
                       'send_evidence', jsonb_build_object(
                           'source_id', $7::text,
                           'recipient_reason', $8::text)),
                   $6, at.trace_id, at.causation_id, at.id
            FROM autopilot_actions at
            WHERE at.id = $9
            RETURNING id
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(campaign.0)
        .bind(&campaign_slug)
        .bind(segment_id)
        .bind(template_key)
        .bind(now)
        .bind(format!("source-campaign:{campaign_slug}"))
        .bind("marketing-consent segment — every fan in it opted in")
        .bind(action_id.into_uuid())
        .fetch_one(&mut **tx)
        .await
        .map_err(map_sqlx)?;
        sqlx::query(
            "UPDATE communication_campaigns \
             SET status = 'scheduled', scheduled_at = $3, dispatch_event_id = $4 \
             WHERE workspace_id = $1 AND id = $2 AND status = 'draft'",
        )
        .bind(workspace_id.into_uuid())
        .bind(campaign.0)
        .bind(now)
        .bind(outbox_id)
        .execute(&mut **tx)
        .await
        .map_err(map_sqlx)?;
    } else if !matches!(campaign.1.as_str(), "scheduled" | "completed") {
        return Err(RepositoryError::Conflict);
    }
    Ok(())
}
