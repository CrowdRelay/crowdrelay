//! Revalidated execution helpers for deterministic action intents.

use super::*;

pub(in crate::autopilot) async fn execute_audience_campaign(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    event_id: EventId,
    phase: crowdrelay_domain::campaign_lifecycle::EventCampaignPhase,
    template_key: &str,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let feature=sqlx::query_scalar::<_,bool>("SELECT COALESCE((SELECT enabled FROM ecosystem_feature_flags WHERE workspace_id=$1 AND key='communication_campaigns_enabled'),false)")
        .bind(workspace_id.into_uuid()).fetch_one(&mut **tx).await.map_err(map_sqlx)?;
    if !feature {
        return Err(RepositoryError::Conflict);
    }
    let row=sqlx::query_as::<_,(String,String,Option<String>)>(r#"
      SELECT event.slug,event.title,city.slug
      FROM events event LEFT JOIN cities city ON city.id=event.city_id
      WHERE event.workspace_id=$1 AND event.id=$2 AND event.status IN ('published','completed') FOR UPDATE OF event
    "#).bind(workspace_id.into_uuid()).bind(event_id.into_uuid()).fetch_optional(&mut **tx).await.map_err(map_sqlx)?.ok_or(RepositoryError::Conflict)?;
    let phase_key = campaign_phase_str(phase);
    let segment_slug = format!("viryaos-{}-{}", row.0, phase_key);
    let campaign_slug = segment_slug.clone();
    let filter = match phase {
        crowdrelay_domain::campaign_lifecycle::EventCampaignPhase::Announcement => {
            json!({"statuses":["active"],"city_slugs":row.2.clone().into_iter().collect::<Vec<_>>(),"marketing_consent":true})
        }
        crowdrelay_domain::campaign_lifecycle::EventCampaignPhase::InterestReminder
        | crowdrelay_domain::campaign_lifecycle::EventCampaignPhase::LastCall => {
            json!({"statuses":["active"],"interested_event_slugs":[row.0.clone()],"excluded_purchased_event_slugs":[row.0.clone()],"marketing_consent":true})
        }
        crowdrelay_domain::campaign_lifecycle::EventCampaignPhase::DayOf => {
            json!({"statuses":["active"],"purchased_event_slugs":[row.0.clone()],"marketing_consent":true})
        }
        crowdrelay_domain::campaign_lifecycle::EventCampaignPhase::ThankYou => {
            json!({"statuses":["active"],"attended_event_slugs":[row.0.clone()],"marketing_consent":true})
        }
    };
    let segment_id = sqlx::query_scalar::<_, Uuid>(
        r#"
      INSERT INTO audience_segments(workspace_id,slug,name,description,filter,active)
      VALUES($1,$2,$3,'ViryaOS managed lifecycle segment',$4,true)
      ON CONFLICT(workspace_id,slug) DO UPDATE SET filter=EXCLUDED.filter,active=true
      RETURNING id
    "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&segment_slug)
    .bind(format!("{} · {}", row.1, phase_key))
    .bind(filter)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    let campaign=sqlx::query_as::<_,(Uuid,String,Option<OffsetDateTime>)>(r#"
      INSERT INTO communication_campaigns(workspace_id,segment_id,slug,name,channel,template_key,content)
      VALUES($1,$2,$3,$4,'email',$5,jsonb_build_object('event_id',$6::uuid,'managed_by','viryaos'))
      ON CONFLICT(workspace_id,slug) DO UPDATE SET template_key=communication_campaigns.template_key
      RETURNING id,status,scheduled_at
    "#).bind(workspace_id.into_uuid()).bind(segment_id).bind(&campaign_slug).bind(format!("{} · {}",row.1,phase_key)).bind(template_key).bind(event_id.into_uuid()).fetch_one(&mut **tx).await.map_err(map_sqlx)?;
    if campaign.1 == "draft" {
        let outbox_id=sqlx::query_scalar::<_,Uuid>(r#"
          INSERT INTO outbox_events(workspace_id,event_type,event_version,payload,available_at)
          VALUES($1,'communication.campaign_due',1,jsonb_build_object('campaign_id',$2::uuid,'campaign_slug',$3::text,'channel','email','segment_id',$4::uuid,'template_key',$5::text),$6)
          RETURNING id
        "#).bind(workspace_id.into_uuid()).bind(campaign.0).bind(&campaign_slug).bind(segment_id).bind(template_key).bind(now).fetch_one(&mut **tx).await.map_err(map_sqlx)?;
        sqlx::query("UPDATE communication_campaigns SET status='scheduled',scheduled_at=$3,dispatch_event_id=$4 WHERE workspace_id=$1 AND id=$2 AND status='draft'")
            .bind(workspace_id.into_uuid()).bind(campaign.0).bind(now).bind(outbox_id).execute(&mut **tx).await.map_err(map_sqlx)?;
    } else if !matches!(campaign.1.as_str(), "scheduled" | "completed") {
        return Err(RepositoryError::Conflict);
    }
    sqlx::query(r#"INSERT INTO viryaos_campaign_lifecycle_emissions(workspace_id,event_id,phase,communication_campaign_id,action_id,emitted_at) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(workspace_id,event_id,phase) DO NOTHING"#)
      .bind(workspace_id.into_uuid()).bind(event_id.into_uuid()).bind(phase_key).bind(campaign.0).bind(action_id.into_uuid()).bind(now).execute(&mut **tx).await.map_err(map_sqlx)?;
    Ok(())
}

pub(in crate::autopilot) async fn lock_outreach_for_execution(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    opportunity_id: OutreachOpportunityId,
    target_id: OutreachTargetId,
    target_version: i64,
) -> Result<(String, String, String), RepositoryError> {
    sqlx::query_as::<_,(String,String,String)>(r#"
      SELECT target.display_name,target.contact_email,opportunity.template_key
      FROM viryaos_outreach_opportunities opportunity JOIN viryaos_outreach_targets target
        ON target.workspace_id=opportunity.workspace_id AND target.id=opportunity.target_id
      WHERE opportunity.workspace_id=$1 AND opportunity.id=$2 AND opportunity.target_id=$3
        AND opportunity.active AND opportunity.expires_at>now()
        AND target.version=$4 AND target.active AND target.verified AND target.accepts_outreach AND NOT target.do_not_contact
      FOR UPDATE OF opportunity,target
    "#).bind(workspace_id.into_uuid()).bind(opportunity_id.into_uuid()).bind(target_id.into_uuid()).bind(target_version)
      .fetch_optional(&mut **tx).await.map_err(map_sqlx)?.ok_or(RepositoryError::Conflict)
}

pub(in crate::autopilot) async fn record_outreach_sent(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    opportunity_id: OutreachOpportunityId,
    target_id: OutreachTargetId,
    phase: crowdrelay_domain::outreach::OutreachPhase,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let followup = matches!(phase, crowdrelay_domain::outreach::OutreachPhase::FollowUp);
    sqlx::query("UPDATE viryaos_outreach_targets SET last_outreach_at=$3,followup_count=CASE WHEN $4 THEN followup_count+1 ELSE 0 END WHERE workspace_id=$1 AND id=$2")
      .bind(workspace_id.into_uuid()).bind(target_id.into_uuid()).bind(now).bind(followup).execute(&mut **tx).await.map_err(map_sqlx)?;
    sqlx::query(r#"INSERT INTO viryaos_outreach_interactions(workspace_id,target_id,opportunity_id,direction,phase,source_key,occurred_at) VALUES($1,$2,$3,'outbound',$4,$5,$6) ON CONFLICT(workspace_id,target_id,source_key) DO NOTHING"#)
      .bind(workspace_id.into_uuid()).bind(target_id.into_uuid()).bind(opportunity_id.into_uuid()).bind(outreach_phase_str(phase)).bind(format!("autopilot:{}",action_id)).bind(now).execute(&mut **tx).await.map_err(map_sqlx)?;
    Ok(())
}

pub(in crate::autopilot) async fn load_content_source_for_execution(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    source_id: ContentSourceId,
    version: i64,
) -> Result<(String, String, Value), RepositoryError> {
    sqlx::query_as::<_,(String,String,Value)>("SELECT source_kind,title,metadata FROM viryaos_content_sources WHERE workspace_id=$1 AND id=$2 AND version=$3 AND active AND expires_at>now() FOR UPDATE")
      .bind(workspace_id.into_uuid()).bind(source_id.into_uuid()).bind(version).fetch_optional(&mut **tx).await.map_err(map_sqlx)?.ok_or(RepositoryError::Conflict)
}

pub(in crate::autopilot) async fn execute_experiment_adjustment(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    experiment_id: ExperimentId,
    expected_version: i64,
    winner: ExperimentVariantId,
    allocations: &[crowdrelay_application::autopilot::ExperimentAllocation],
    complete: bool,
) -> Result<(), RepositoryError> {
    let locked=sqlx::query_scalar::<_,i64>("SELECT version FROM viryaos_experiments WHERE workspace_id=$1 AND id=$2 AND version=$3 AND status='running' FOR UPDATE")
      .bind(workspace_id.into_uuid()).bind(experiment_id.into_uuid()).bind(expected_version).fetch_optional(&mut **tx).await.map_err(map_sqlx)?.ok_or(RepositoryError::Conflict)?;
    let total: u32 = allocations
        .iter()
        .map(|a| u32::from(a.allocation_basis_points))
        .sum();
    if allocations.len() < 2 || total != 10_000 || locked != expected_version {
        return Err(RepositoryError::Conflict);
    }
    for allocation in allocations {
        let changed=sqlx::query("UPDATE viryaos_experiment_variants SET allocation_basis_points=$4,version=version+1 WHERE workspace_id=$1 AND experiment_id=$2 AND id=$3 AND active")
          .bind(workspace_id.into_uuid()).bind(experiment_id.into_uuid()).bind(allocation.variant_id.into_uuid()).bind(i32::from(allocation.allocation_basis_points)).execute(&mut **tx).await.map_err(map_sqlx)?;
        if changed.rows_affected() != 1 {
            return Err(RepositoryError::Conflict);
        }
    }
    let status = if complete { "completed" } else { "running" };
    let changed=sqlx::query("UPDATE viryaos_experiments SET status=$4,winner_variant_id=CASE WHEN $5 THEN $3 ELSE winner_variant_id END,version=version+1 WHERE workspace_id=$1 AND id=$2 AND version=$6")
      .bind(workspace_id.into_uuid()).bind(experiment_id.into_uuid()).bind(winner.into_uuid()).bind(status).bind(complete).bind(expected_version).execute(&mut **tx).await.map_err(map_sqlx)?;
    if changed.rows_affected() != 1 {
        return Err(RepositoryError::Conflict);
    }
    Ok(())
}

pub(in crate::autopilot) async fn complete_show_task(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    event_id: EventId,
    task: crowdrelay_domain::show_operations::ShowTaskKind,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let fact=sqlx::query_scalar::<_,bool>(r#"
      SELECT CASE $3
        WHEN 'announcement_published' THEN event.status IN ('published','completed')
        WHEN 'ticketing_verified' THEN EXISTS(SELECT 1 FROM ticket_sales sale WHERE sale.workspace_id=event.workspace_id AND sale.event_id=event.id AND sale.active AND sale.sales_open_at<sale.sales_close_at AND EXISTS(SELECT 1 FROM ticket_types type WHERE type.workspace_id=sale.workspace_id AND type.ticket_sale_id=sale.id AND type.active))
        ELSE false END
      FROM events event WHERE event.workspace_id=$1 AND event.id=$2 FOR UPDATE
    "#).bind(workspace_id.into_uuid()).bind(event_id.into_uuid()).bind(task.key()).fetch_optional(&mut **tx).await.map_err(map_sqlx)?.ok_or(RepositoryError::Conflict)?;
    if !fact || task.is_physical() {
        return Err(RepositoryError::Conflict);
    }
    sqlx::query(r#"INSERT INTO show_checklist_items(workspace_id,event_id,item_key,status,note,updated_at) VALUES($1,$2,$3,'done','Verified automatically by ViryaOS from first-party state',$4) ON CONFLICT(workspace_id,event_id,item_key) DO UPDATE SET status='done',note=EXCLUDED.note,updated_at=EXCLUDED.updated_at WHERE show_checklist_items.status<>'done'"#)
      .bind(workspace_id.into_uuid()).bind(event_id.into_uuid()).bind(task.key()).bind(now).execute(&mut **tx).await.map_err(map_sqlx)?;
    Ok(())
}

pub(in crate::autopilot) const fn campaign_phase_str(
    v: crowdrelay_domain::campaign_lifecycle::EventCampaignPhase,
) -> &'static str {
    use crowdrelay_domain::campaign_lifecycle::EventCampaignPhase::*;
    match v {
        Announcement => "announcement",
        InterestReminder => "interest_reminder",
        LastCall => "last_call",
        DayOf => "day_of",
        ThankYou => "thank_you",
    }
}
pub(in crate::autopilot) const fn outreach_phase_str(
    v: crowdrelay_domain::outreach::OutreachPhase,
) -> &'static str {
    match v {
        crowdrelay_domain::outreach::OutreachPhase::Initial => "initial",
        crowdrelay_domain::outreach::OutreachPhase::FollowUp => "followup",
    }
}
