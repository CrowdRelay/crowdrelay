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
    if matches!(
        phase,
        crowdrelay_domain::campaign_lifecycle::EventCampaignPhase::Announcement
    ) {
        sqlx::query(r#"
          INSERT INTO viryaos_outreach_opportunities(
            workspace_id,target_id,source,subject_kind,subject_key,template_key,
            relevance_basis_points,confidence_basis_points,active,observed_at,expires_at)
          SELECT target.workspace_id,target.id,'event_autopilot','event',$2,
            CASE target.target_kind
              WHEN 'media_patronage' THEN 'event.media_patronage.v1'
              WHEN 'endorsement' THEN 'event.endorsement.v1'
              ELSE 'event.press.v1'
            END,
            GREATEST(7000,LEAST(10000,target.relationship_score*100)),8800,true,$3,$3 + INTERVAL '30 days'
          FROM viryaos_outreach_targets target
          WHERE target.workspace_id=$1 AND target.active AND target.verified AND target.accepts_outreach AND NOT target.do_not_contact
            AND target.target_kind IN ('press','radio','creator','media_patronage','endorsement')
          ON CONFLICT(workspace_id,source,target_id,subject_kind,subject_key) DO UPDATE SET
            active=true,observed_at=EXCLUDED.observed_at,expires_at=EXCLUDED.expires_at,
            relevance_basis_points=EXCLUDED.relevance_basis_points,confidence_basis_points=EXCLUDED.confidence_basis_points
        "#).bind(workspace_id.into_uuid()).bind(format!("event:{}",event_id)).bind(now)
          .execute(&mut **tx).await.map_err(map_sqlx)?;
    }
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
    sqlx::query("UPDATE viryaos_outreach_targets SET last_outreach_at=$3,followup_count=CASE WHEN $4 THEN followup_count+1 ELSE 0 END,contact_verified_at=CASE WHEN contact_verified_at IS NULL OR contact_verified_at < $3 THEN $3 ELSE contact_verified_at END WHERE workspace_id=$1 AND id=$2")
      .bind(workspace_id.into_uuid()).bind(target_id.into_uuid()).bind(now).bind(followup).execute(&mut **tx).await.map_err(map_sqlx)?;
    sqlx::query(r#"INSERT INTO viryaos_outreach_interactions(workspace_id,target_id,opportunity_id,direction,phase,source_key,occurred_at) VALUES($1,$2,$3,'outbound',$4,$5,$6) ON CONFLICT(workspace_id,target_id,source_key) DO NOTHING"#)
      .bind(workspace_id.into_uuid()).bind(target_id.into_uuid()).bind(opportunity_id.into_uuid()).bind(outreach_phase_str(phase)).bind(format!("autopilot:{}",action_id)).bind(now).execute(&mut **tx).await.map_err(map_sqlx)?;
    Ok(())
}

pub(in crate::autopilot) async fn load_content_source_for_execution(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    source_id: ContentSourceId,
) -> Result<(String, String, Value), RepositoryError> {
    // The decision-time source_version is deliberately not re-checked here:
    // measurement refreshes bump the version while an action waits out its
    // execution delay, and the emitted payload ships this freshly read row
    // anyway, so a pinned version only converted routine churn into permanent
    // action failures. Liveness (active, unexpired) is the real gate.
    sqlx::query_as::<_,(String,String,Value)>("SELECT source_kind,title,metadata FROM viryaos_content_sources WHERE workspace_id=$1 AND id=$2 AND active AND expires_at>now() FOR UPDATE")
      .bind(workspace_id.into_uuid()).bind(source_id.into_uuid()).fetch_optional(&mut **tx).await.map_err(map_sqlx)?.ok_or(RepositoryError::Conflict)
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

#[allow(clippy::too_many_arguments)]
pub(in crate::autopilot) async fn execute_release_milestone(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    release_id: crowdrelay_domain::ReleasePlanId,
    title: &str,
    release_at: OffsetDateTime,
    milestone: crowdrelay_domain::release_autopilot::ReleaseMilestone,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let locked = sqlx::query_as::<
        _,
        (
            String,
            OffsetDateTime,
            bool,
            bool,
            bool,
            String,
            Option<String>,
        ),
    >(
        r#"
        SELECT title,release_at,active,communication_enabled,press_enabled,source_key,listen_url
        FROM viryaos_release_plans WHERE workspace_id=$1 AND id=$2 FOR UPDATE
    "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(release_id.into_uuid())
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::Conflict)?;
    if !locked.2 || locked.1 != release_at || locked.0 != title {
        return Err(RepositoryError::Conflict);
    }
    let key = release_milestone_str(milestone);
    let already=sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM viryaos_release_milestones WHERE workspace_id=$1 AND release_id=$2 AND milestone=$3)")
        .bind(workspace_id.into_uuid()).bind(release_id.into_uuid()).bind(key).fetch_one(&mut **tx).await.map_err(map_sqlx)?;
    if already {
        return Ok(());
    }

    // Same rule as a show: nothing gets shared before there is a link that can
    // be counted. Run for every milestone rather than only the first, because
    // the first one can legitimately fail on a missing executor capability and
    // the announcement must not then go out untracked. It is an upsert, so the
    // repeats cost a statement and change nothing.
    ensure_release_tracked_link(tx, workspace_id, &locked.5, locked.6.as_deref()).await?;

    use crowdrelay_domain::release_autopilot::ReleaseMilestone::*;
    match milestone {
        SeedCalendar => {
            seed_release_calendar(tx, workspace_id, action_id, release_id, title, release_at)
                .await?
        }
        // Assembling the pitch and putting it in front of somebody. The form
        // itself has no API, so the milestone records that the task exists and
        // stops there; claiming a submission would be claiming a fiction.
        EditorialPitch => {
            crate::autopilot::emit_external_action(
                tx,
                workspace_id,
                action_id,
                "crowdrelay.release.editorial_pitch_parked",
                json!({
                    "action_id": action_id,
                    "release_id": release_id,
                    "title": title,
                    "release_at": release_at,
                    "submitted_by_agent": false,
                }),
            )
            .await?;
        }
        StartPress => {
            if !locked.4 {
                return Err(RepositoryError::Conflict);
            }
            seed_release_outreach(tx, workspace_id, release_id, title, release_at, now).await?;
        }
        Announcement | FanWarmup | Countdown | ReleaseDay | Sustain | Wrap => {
            if !locked.3 {
                return Err(RepositoryError::Conflict);
            }
            execute_release_campaign(
                tx,
                workspace_id,
                action_id,
                release_id,
                title,
                milestone,
                now,
            )
            .await?;
        }
    }
    sqlx::query(r#"INSERT INTO viryaos_release_milestones(workspace_id,release_id,milestone,action_id,completed_at) VALUES($1,$2,$3,$4,$5) ON CONFLICT(workspace_id,release_id,milestone) DO NOTHING"#)
        .bind(workspace_id.into_uuid()).bind(release_id.into_uuid()).bind(key).bind(action_id.into_uuid()).bind(now)
        .execute(&mut **tx).await.map_err(map_sqlx)?;
    Ok(())
}

async fn seed_release_calendar(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    release_id: crowdrelay_domain::ReleasePlanId,
    title: &str,
    release_at: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let milestones = [
        ("announcement", -28_i64, "Announcement"),
        ("press", -21, "Press pitching"),
        ("fan-warmup", -14, "Fan warm-up"),
        ("countdown", -7, "Countdown"),
        ("release-day", 0, "Release day"),
        ("sustain", 3, "Sustain wave"),
        ("wrap", 14, "Campaign wrap"),
    ];
    for (slug, days, label) in milestones {
        let calendar_key = format!("release:{}:{}", release_id, slug);
        let exists=sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM viryaos_calendar_requests WHERE workspace_id=$1 AND calendar_key=$2)")
            .bind(workspace_id.into_uuid()).bind(&calendar_key).fetch_one(&mut **tx).await.map_err(map_sqlx)?;
        if exists {
            continue;
        }
        crate::autopilot::ensure_executor_capability(tx, workspace_id, "calendar.upsert").await?;
        let outbox_id = Uuid::now_v7();
        let starts_at = release_at + time::Duration::days(days);
        sqlx::query(r#"INSERT INTO outbox_events(id,workspace_id,event_type,event_version,payload,request_id,max_attempts) VALUES($1,$2,'crowdrelay.calendar.upsert_requested',1,$3,$4,12)"#)
          .bind(outbox_id).bind(workspace_id.into_uuid()).bind(json!({"action_id":action_id,"calendar_key":calendar_key,"title":format!("VIRYA · {title} · {label}"),"starts_at":starts_at,"source_kind":"release","source_id":release_id})).bind(format!("autopilot-action:{action_id}:{slug}"))
          .execute(&mut **tx).await.map_err(map_sqlx)?;
        sqlx::query(r#"INSERT INTO viryaos_calendar_requests(workspace_id,source_kind,source_id,calendar_key,title,starts_at,action_id,outbox_event_id) VALUES($1,'release',$2,$3,$4,$5,$6,$7)"#)
          .bind(workspace_id.into_uuid()).bind(release_id.into_uuid()).bind(&calendar_key).bind(format!("VIRYA · {title} · {label}")).bind(starts_at).bind(action_id.into_uuid()).bind(outbox_id)
          .execute(&mut **tx).await.map_err(map_sqlx)?;
    }
    Ok(())
}

async fn execute_release_campaign(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    release_id: crowdrelay_domain::ReleasePlanId,
    title: &str,
    milestone: crowdrelay_domain::release_autopilot::ReleaseMilestone,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let feature=sqlx::query_scalar::<_,bool>("SELECT COALESCE((SELECT enabled FROM ecosystem_feature_flags WHERE workspace_id=$1 AND key='communication_campaigns_enabled'),false)")
      .bind(workspace_id.into_uuid()).fetch_one(&mut **tx).await.map_err(map_sqlx)?;
    if !feature {
        return Err(RepositoryError::Conflict);
    }
    let phase = release_milestone_str(milestone);
    let segment_slug = format!("viryaos-release-{}", release_id);
    let campaign_slug = format!("viryaos-release-{}-{}", release_id, phase);
    let segment_id=sqlx::query_scalar::<_,Uuid>(r#"INSERT INTO audience_segments(workspace_id,slug,name,description,filter,active) VALUES($1,$2,$3,'VIRYA OS release audience',jsonb_build_object('statuses',jsonb_build_array('active'),'marketing_consent',true),true) ON CONFLICT(workspace_id,slug) DO UPDATE SET active=true RETURNING id"#)
      .bind(workspace_id.into_uuid()).bind(&segment_slug).bind(format!("{title} · release audience")).fetch_one(&mut **tx).await.map_err(map_sqlx)?;
    let template = format!("release.{phase}.v1");
    let growth_goal = match milestone {
        crowdrelay_domain::release_autopilot::ReleaseMilestone::FanWarmup => "referral",
        crowdrelay_domain::release_autopilot::ReleaseMilestone::Wrap => "retention",
        _ => "engagement",
    };
    let campaign = sqlx::query_as::<_, (Uuid, String)>(
        r#"
        INSERT INTO communication_campaigns(
            workspace_id,segment_id,slug,name,channel,template_key,content
        ) VALUES(
            $1,$2,$3,$4,'email',$5,
            jsonb_build_object(
                'release_id',$6::uuid,
                'managed_by','viryaos',
                'growth_goal',$7::text
            )
        )
        ON CONFLICT(workspace_id,slug)
        DO UPDATE SET template_key=communication_campaigns.template_key
        RETURNING id,status
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(segment_id)
    .bind(&campaign_slug)
    .bind(format!("{title} · {phase}"))
    .bind(&template)
    .bind(release_id.into_uuid())
    .bind(growth_goal)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    if campaign.1 == "draft" {
        let outbox_id=sqlx::query_scalar::<_,Uuid>(r#"INSERT INTO outbox_events(workspace_id,event_type,event_version,payload,available_at) VALUES($1,'communication.campaign_due',1,jsonb_build_object('campaign_id',$2::uuid,'campaign_slug',$3::text,'channel','email','segment_id',$4::uuid,'template_key',$5::text),$6) RETURNING id"#)
          .bind(workspace_id.into_uuid()).bind(campaign.0).bind(&campaign_slug).bind(segment_id).bind(&template).bind(now).fetch_one(&mut **tx).await.map_err(map_sqlx)?;
        sqlx::query("UPDATE communication_campaigns SET status='scheduled',scheduled_at=$3,dispatch_event_id=$4 WHERE workspace_id=$1 AND id=$2 AND status='draft'")
          .bind(workspace_id.into_uuid()).bind(campaign.0).bind(now).bind(outbox_id).execute(&mut **tx).await.map_err(map_sqlx)?;
    } else if !matches!(campaign.1.as_str(), "scheduled" | "completed") {
        return Err(RepositoryError::Conflict);
    }
    let _ = action_id;
    Ok(())
}

async fn seed_release_outreach(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    release_id: crowdrelay_domain::ReleasePlanId,
    _title: &str,
    release_at: OffsetDateTime,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    sqlx::query(r#"
      INSERT INTO viryaos_outreach_opportunities(
        workspace_id,target_id,source,subject_kind,subject_key,template_key,
        relevance_basis_points,confidence_basis_points,active,observed_at,expires_at)
      SELECT target.workspace_id,target.id,'release_autopilot','release',$2,
        CASE target.target_kind
          WHEN 'media_patronage' THEN 'release.media_patronage.v1'
          WHEN 'endorsement' THEN 'release.endorsement.v1'
          ELSE 'release.press.v1' END,
        GREATEST(7000,LEAST(10000,target.relationship_score*100)),9000,true,$3,GREATEST($4 + INTERVAL '14 days',$3 + INTERVAL '14 days')
      FROM viryaos_outreach_targets target
      WHERE target.workspace_id=$1 AND target.active AND target.verified AND target.accepts_outreach AND NOT target.do_not_contact
        AND target.target_kind IN ('press','radio','creator','media_patronage','endorsement')
      ON CONFLICT(workspace_id,source,target_id,subject_kind,subject_key) DO UPDATE SET
        active=true,observed_at=EXCLUDED.observed_at,expires_at=EXCLUDED.expires_at,
        relevance_basis_points=EXCLUDED.relevance_basis_points,confidence_basis_points=EXCLUDED.confidence_basis_points
    "#).bind(workspace_id.into_uuid()).bind(format!("release:{release_id}")).bind(now).bind(release_at)
      .execute(&mut **tx).await.map_err(map_sqlx)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn seed_deadline_calendar(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    source_kind: &'static str,
    source_id: Uuid,
    calendar_key: &str,
    title: &str,
    starts_at: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM viryaos_calendar_requests WHERE workspace_id=$1 AND calendar_key=$2)",
    )
    .bind(workspace_id.into_uuid())
    .bind(calendar_key)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    if exists {
        return Ok(());
    }
    match crate::autopilot::ensure_executor_capability(tx, workspace_id, "calendar.upsert").await {
        Ok(()) => {}
        // Deadline reminders enrich opportunity/funding execution but are not
        // allowed to block the primary provider action. The dedicated release
        // SeedCalendar milestone remains strict and surfaces missing capability.
        Err(RepositoryError::Unavailable) => return Ok(()),
        Err(error) => return Err(error),
    }
    let outbox_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO outbox_events(id,workspace_id,event_type,event_version,payload,request_id,max_attempts)
           VALUES($1,$2,'crowdrelay.calendar.upsert_requested',1,$3,$4,12)"#,
    )
    .bind(outbox_id)
    .bind(workspace_id.into_uuid())
    .bind(json!({
        "action_id": action_id,
        "calendar_key": calendar_key,
        "title": title,
        "starts_at": starts_at,
        "source_kind": source_kind,
        "source_id": source_id,
    }))
    .bind(format!("autopilot-action:{action_id}:calendar"))
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    sqlx::query(
        r#"INSERT INTO viryaos_calendar_requests(
              workspace_id,source_kind,source_id,calendar_key,title,starts_at,action_id,outbox_event_id
           ) VALUES($1,$2,$3,$4,$5,$6,$7,$8)"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(source_kind)
    .bind(source_id)
    .bind(calendar_key)
    .bind(title)
    .bind(starts_at)
    .bind(action_id.into_uuid())
    .bind(outbox_id)
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

pub(in crate::autopilot) async fn execute_live_opportunity(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    opportunity_id: crowdrelay_domain::TeamOpportunityId,
    kind: crowdrelay_domain::live_opportunities::LiveOpportunityKind,
    score: u16,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let row = sqlx::query_as::<
        _,
        (
            String,
            String,
            Option<String>,
            Option<String>,
            String,
            i64,
            i64,
            i64,
            bool,
            bool,
            Option<OffsetDateTime>,
        ),
    >(
        r#"
        SELECT title, organization, destination_url, contact_email, currency,
               expected_fee_minor, estimated_cost_minor, application_fee_minor,
               requires_contract, exclusive, deadline
        FROM viryaos_team_opportunities
        WHERE workspace_id=$1 AND id=$2 AND eligible AND verified_destination
          AND status IN ('new','prepared','awaiting_approval')
        FOR UPDATE
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(opportunity_id.into_uuid())
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::Conflict)?;

    if let Some(deadline) = row.10 {
        seed_deadline_calendar(
            tx,
            workspace_id,
            action_id,
            "opportunity",
            opportunity_id.into_uuid(),
            &format!("opportunity:{opportunity_id}:deadline"),
            &format!("VIRYA · application deadline · {}", row.0),
            deadline,
        )
        .await?;
    }

    crate::autopilot::emit_external_action(
        tx,
        workspace_id,
        action_id,
        "crowdrelay.opportunity.application_requested",
        json!({
            "action_id": action_id,
            "opportunity_id": opportunity_id,
            "kind": kind,
            "score": score,
            "title": row.0,
            "organization": row.1,
            "destination_url": row.2,
            "contact_email": row.3,
            "currency": row.4,
            "expected_fee_minor": row.5,
            "estimated_cost_minor": row.6,
            "application_fee_minor": row.7,
            "requires_contract": row.8,
            "exclusive": row.9,
            "deadline": row.10,
            "payment_execution_allowed": false,
        }),
    )
    .await?;

    sqlx::query(
        "UPDATE viryaos_team_opportunities \
         SET status='submission_requested', last_action_at=$3, version=version+1 \
         WHERE workspace_id=$1 AND id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(opportunity_id.into_uuid())
    .bind(now)
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

/// Sends one negotiation move to whoever actually talks to the promoter.
///
/// The row is locked and re-read rather than trusted from the payload. Time
/// passes between a decision and its execution, and in that gap an operator may
/// have recorded a better offer, the promoter may have withdrawn, or the window
/// may have closed — all of which make the drafted move the wrong one to send.
///
/// `accepted` is the only state this writes that ends the negotiation. A
/// counter leaves it `countered`, which is the agent waiting rather than the
/// agent finished.
pub(in crate::autopilot) struct TermsMove<'a> {
    pub opportunity_id: crowdrelay_domain::TeamOpportunityId,
    /// True for an acceptance, false for a counter. Two booleans' worth of
    /// behaviour hangs off this, and both are stated at the call site.
    pub accept: bool,
    pub amount_minor: i64,
    pub currency: &'a str,
    pub round: u8,
}

pub(in crate::autopilot) async fn execute_live_opportunity_terms(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    request: &TermsMove<'_>,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let TermsMove {
        opportunity_id,
        accept,
        amount_minor,
        currency,
        round,
    } = *request;
    let row = sqlx::query_as::<_, (String, String, Option<String>, i64, i64, String, i32)>(
        r#"
        SELECT opportunity.title, opportunity.organization, opportunity.contact_email,
               terms.offered_fee_minor, terms.walk_away_minor, terms.currency,
               terms.counter_rounds
        FROM viryaos_team_opportunity_terms AS terms
        JOIN viryaos_team_opportunities AS opportunity
          ON opportunity.workspace_id = terms.workspace_id
         AND opportunity.id = terms.opportunity_id
        WHERE terms.workspace_id = $1
          AND terms.opportunity_id = $2
          AND terms.settled_at IS NULL
          AND terms.responds_by > $3
        FOR UPDATE OF terms
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(opportunity_id.into_uuid())
    .bind(now)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::Conflict)?;
    // A move quoted in a currency the negotiation is not conducted in is a
    // different offer, not a rounding difference.
    if row.5 != currency {
        return Err(RepositoryError::Conflict);
    }
    // The floor is the one number that must hold at execution as well as at
    // decision. Everything else here can change harmlessly; accepting below
    // cost cannot.
    if accept && amount_minor < row.4 {
        return Err(RepositoryError::Conflict);
    }

    crate::autopilot::emit_external_action(
        tx,
        workspace_id,
        action_id,
        if accept {
            "crowdrelay.opportunity.terms_accepted"
        } else {
            "crowdrelay.opportunity.terms_countered"
        },
        json!({
            "action_id": action_id,
            "opportunity_id": opportunity_id,
            "title": row.0,
            "organization": row.1,
            "contact_email": row.2,
            "currency": currency,
            "offered_fee_minor": row.3,
            "walk_away_minor": row.4,
            "amount_minor": amount_minor,
            "round": round,
            "payment_execution_allowed": false,
        }),
    )
    .await?;

    if accept {
        sqlx::query(
            "UPDATE viryaos_team_opportunity_terms \
             SET state='accepted', settled_at=$3, version=version+1 \
             WHERE workspace_id=$1 AND opportunity_id=$2 AND settled_at IS NULL",
        )
        .bind(workspace_id.into_uuid())
        .bind(opportunity_id.into_uuid())
        .bind(now)
        .execute(&mut **tx)
        .await
        .map_err(map_sqlx)?;
    } else {
        // `counter_rounds + 1` from the row rather than from the payload: two
        // executions of the same drafted counter must not count as two asks.
        sqlx::query(
            "UPDATE viryaos_team_opportunity_terms \
             SET state='countered', countered_fee_minor=$3, counter_rounds=counter_rounds+1, \
                 version=version+1 \
             WHERE workspace_id=$1 AND opportunity_id=$2 AND settled_at IS NULL",
        )
        .bind(workspace_id.into_uuid())
        .bind(opportunity_id.into_uuid())
        .bind(amount_minor)
        .execute(&mut **tx)
        .await
        .map_err(map_sqlx)?;
    }
    Ok(())
}

pub(in crate::autopilot) async fn prepare_funding_package(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    opportunity_id: crowdrelay_domain::TeamOpportunityId,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let row = sqlx::query_as::<
        _,
        (
            String,
            String,
            Option<String>,
            String,
            i64,
            i64,
            OffsetDateTime,
            Value,
        ),
    >(
        r#"
        SELECT title, organization, destination_url, currency, funding_amount_minor,
               own_contribution_minor, deadline, metadata
        FROM viryaos_team_opportunities
        WHERE workspace_id=$1 AND id=$2 AND opportunity_kind='funding' AND eligible
          AND package_status IN ('none','requested') AND status IN ('new','prepared')
        FOR UPDATE
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(opportunity_id.into_uuid())
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::Conflict)?;

    seed_deadline_calendar(
        tx,
        workspace_id,
        action_id,
        "funding",
        opportunity_id.into_uuid(),
        &format!("funding:{opportunity_id}:deadline"),
        &format!("VIRYA · funding deadline · {}", row.0),
        row.6,
    )
    .await?;

    crate::autopilot::emit_external_action(
        tx,
        workspace_id,
        action_id,
        "crowdrelay.funding.package_requested",
        json!({
            "action_id": action_id,
            "opportunity_id": opportunity_id,
            "title": row.0,
            "organization": row.1,
            "destination_url": row.2,
            "currency": row.3,
            "funding_amount_minor": row.4,
            "own_contribution_minor": row.5,
            "deadline": row.6,
            "facts": row.7,
            "generator": "deterministic_template",
        }),
    )
    .await?;

    sqlx::query(
        "UPDATE viryaos_team_opportunities \
         SET package_status='requested', status='prepared', last_action_at=$3, version=version+1 \
         WHERE workspace_id=$1 AND id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(opportunity_id.into_uuid())
    .bind(now)
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

pub(in crate::autopilot) async fn submit_funding_application(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    opportunity_id: crowdrelay_domain::TeamOpportunityId,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let row = sqlx::query_as::<_, (String, String, Option<String>, String, OffsetDateTime)>(
        r#"
        SELECT title, organization, destination_url, currency, deadline
        FROM viryaos_team_opportunities
        WHERE workspace_id=$1 AND id=$2 AND opportunity_kind='funding' AND eligible
          AND package_status='ready' AND status IN ('prepared','awaiting_approval')
          AND deadline>now()
        FOR UPDATE
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(opportunity_id.into_uuid())
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::Conflict)?;

    crate::autopilot::emit_external_action(
        tx,
        workspace_id,
        action_id,
        "crowdrelay.funding.submission_requested",
        json!({
            "action_id": action_id,
            "opportunity_id": opportunity_id,
            "title": row.0,
            "organization": row.1,
            "destination_url": row.2,
            "currency": row.3,
            "deadline": row.4,
            "human_approved": true,
        }),
    )
    .await?;

    sqlx::query(
        "UPDATE viryaos_team_opportunities \
         SET status='submission_requested', last_action_at=$3, version=version+1 \
         WHERE workspace_id=$1 AND id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(opportunity_id.into_uuid())
    .bind(now)
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

/// Chases an editorial pitch nobody has submitted yet.
///
/// The reminder and the timestamp that keeps the next one a cooldown away are
/// written together, so a chase that went out is always one the schedule knows
/// about. Guarded on the pitch still being open: a reminder about something
/// somebody has already done is how an operator learns to ignore them.
pub(in crate::autopilot) async fn escalate_editorial_pitch(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    release_id: crowdrelay_domain::ReleasePlanId,
    title: &str,
    due_at: OffsetDateTime,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    crate::autopilot::emit_external_action(
        tx,
        workspace_id,
        action_id,
        "crowdrelay.release.editorial_pitch_escalated",
        json!({
            "action_id": action_id,
            "release_id": release_id,
            "title": title,
            "due_at": due_at,
            "submitted_by_agent": false,
        }),
    )
    .await?;
    sqlx::query(
        "UPDATE viryaos_release_plans SET editorial_pitch_escalated_at=$3 \
         WHERE workspace_id=$1 AND id=$2 AND editorial_pitch_completed_at IS NULL",
    )
    .bind(workspace_id.into_uuid())
    .bind(release_id.into_uuid())
    .bind(now)
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

pub(in crate::autopilot) const fn release_milestone_str(
    milestone: crowdrelay_domain::release_autopilot::ReleaseMilestone,
) -> &'static str {
    use crowdrelay_domain::release_autopilot::ReleaseMilestone::*;
    match milestone {
        SeedCalendar => "seed_calendar",
        EditorialPitch => "editorial_pitch",
        Announcement => "announcement",
        StartPress => "start_press",
        FanWarmup => "fan_warmup",
        Countdown => "countdown",
        ReleaseDay => "release_day",
        Sustain => "sustain",
        Wrap => "wrap",
    }
}

/// The deterministic brain dispatches an LLM worker. Execution creates an
/// `agent_service_tasks` row that the TS agent service's scheduler picks up
/// on the next tick. The worker runs the specified template with the
/// deterministic prompt and emits outcomes that the brain consumes.
///
/// This is a first-party reversible write: creating a task row reaches nobody,
/// costs nothing, and is undone by deleting the row. The worker's outcomes
/// flow back through `agent_outcomes` and the existing autopilot mapping.
#[allow(clippy::too_many_arguments)]
pub(in crate::autopilot) async fn execute_agent_run(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    template_id: &str,
    prompt: &str,
    _priority: u8,
    tier: crowdrelay_brain::AgentTier,
    _now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    // Insert a task row into agent_service_tasks. The TS agent service's
    // scheduler claims due tasks and runs them. model_id = "auto" tells the
    // runner to pick a model based on the tier: free for basic, connected
    // premium provider for premium (with fallback to basic if none connected).
    sqlx::query(
        r#"
        INSERT INTO agent_service_tasks (id, workspace_id, template_id, model_id, prompt, status, tier, metadata)
        VALUES ($1, $2, $3, 'auto', $4, 'queued', $5, $6)
        "#,
    )
    .bind(uuid::Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(template_id)
    .bind(prompt)
    .bind(tier.as_str())
    .bind(serde_json::json!({
        "source": "autopilot",
        "action_id": action_id.into_uuid(),
    }))
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

/// Materializes an approved Signal push as `fan_push_deliveries` rows for all
/// consented fans with active push endpoints. The PushDeliveryWorker then
/// sends them via FCM/Web Push.
///
/// Idempotency: `UNIQUE (workspace_id, source_kind, source_id, endpoint_id)`
/// on `fan_push_deliveries` means a retry of the same action + endpoint is a
/// no-op. `source_id` is the autopilot action id.
///
/// When `segment` is `Some(slug)`, the slug is resolved against the
/// `audience_segments` table. If a matching active segment is found, its
/// JSONB filter is parsed into a typed `SegmentFilter` and applied as
/// additional fan predicates so only segment members receive the push.
/// If the slug is not found, inactive, or the filter has invalid field
/// types, the push falls back to broadcasting to all consented fans with
/// active endpoints (the original behavior). See `push_segments.rs` for
/// filter resolution and SQL generation.
#[allow(clippy::too_many_arguments)]
pub(in crate::autopilot) async fn execute_signal_push(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    title: &str,
    body: &str,
    target_path: Option<&str>,
    _event_id: Option<&uuid::Uuid>,
    segment: Option<&str>,
    _now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let action_uuid = action_id.into_uuid();
    let collapse_key = format!("agent:{action_uuid}");
    let target = target_path.unwrap_or("/my-signal/");

    // Resolve the segment filter if a slug was provided. Falls back to
    // None (broadcast) if the slug is missing, not found, inactive, or
    // the filter has invalid field types.
    let mut segment_filter = resolve_segment_filter(tx, workspace_id, segment).await;

    // Build the segment clause + typed bind values. Only fields present
    // in the filter generate SQL conditions, avoiding unnecessary
    // correlated subqueries for absent fields.
    let (segment_clause, segment_binds) = segment_filter
        .as_mut()
        .map(|f| f.sql_clause(7))
        .unwrap_or_default();

    let sql = format!(
        r#"
        INSERT INTO fan_push_deliveries
            (workspace_id, fan_id, endpoint_id, source_kind, source_id,
             category, title, body, target_path, collapse_key)
        SELECT endpoint.workspace_id, endpoint.fan_id, endpoint.id,
               'agent_signal_push', $2,
               'community', $3, $4, $5, $6
        FROM fan_push_endpoints endpoint
        JOIN fans fan
          ON fan.workspace_id = endpoint.workspace_id
         AND fan.id = endpoint.fan_id
        WHERE endpoint.workspace_id = $1
          AND endpoint.active
          AND endpoint.invalidated_at IS NULL
          AND fan.status = 'active'
          AND EXISTS (
              SELECT 1 FROM fan_consents consent
              WHERE consent.workspace_id = endpoint.workspace_id
                AND consent.fan_id = endpoint.fan_id
                AND consent.purpose = 'marketing'
                AND consent.granted
                AND consent.id = (
                    SELECT newest.id FROM fan_consents newest
                    WHERE newest.workspace_id = consent.workspace_id
                      AND newest.fan_id = consent.fan_id
                      AND newest.purpose = consent.purpose
                    ORDER BY newest.recorded_at DESC, newest.id DESC LIMIT 1
                )
          )
          {segment_clause}
        ON CONFLICT (workspace_id, source_kind, source_id, endpoint_id) DO NOTHING
        "#
    );

    let mut query = sqlx::query(&sql)
        .bind(workspace_id.into_uuid())
        .bind(action_uuid)
        .bind(title)
        .bind(body)
        .bind(target)
        .bind(&collapse_key);

    for bind in segment_binds {
        query = match bind {
            SegmentBind::Statuses(v) => query.bind(v),
            SegmentBind::CitySlugs(v) => query.bind(v),
            SegmentBind::MinReferrals(v) => query.bind(v),
            SegmentBind::Synesthesia(v) => query.bind(v),
            SegmentBind::TagsAll(v) => query.bind(v),
        };
    }

    query.execute(&mut **tx).await.map_err(map_sqlx)?;

    Ok(())
}
