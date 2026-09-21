//! The release campaign waves — one segment per rung so the late sends
//! provably subtract the fans an earlier phase already reached (§4i-4).
//! `execution.rs` keeps the milestone dispatch; the audience arithmetic and
//! the likely-listener ranking live here.

use super::*;

pub(in crate::autopilot) async fn execute_release_campaign(
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
    let campaign_slug = format!("crowdrelay-release-{}-{}", release_id, phase);
    // The late waves do not re-send to fans an earlier phase already reached
    // (§4i-4 / Gap 2): "you might have missed it" is only honest when the
    // segment provably excludes the already-contacted. `excluded_campaign_slugs`
    // names the phases that ran before; a phase that never fired contributes a
    // slug no campaign carries, which excludes nothing.
    let (segment_slug, segment_name, segment_filter) = match milestone {
        crowdrelay_domain::release_autopilot::ReleaseMilestone::Wrap
        | crowdrelay_domain::release_autopilot::ReleaseMilestone::CatalogueRotation => (
            format!("crowdrelay-release-{release_id}-missed"),
            format!("{title} · missed the release"),
            json!({
                "statuses": ["active"],
                "marketing_consent": true,
                "excluded_campaign_slugs": earlier_release_campaign_slugs(release_id),
            }),
        ),
        crowdrelay_domain::release_autopilot::ReleaseMilestone::Countdown => (
            format!("crowdrelay-release-{release_id}-likely"),
            format!("{title} · likely listeners"),
            json!({
                "statuses": ["active"],
                "marketing_consent": true,
                "tags_all": [likely_listener_tag(release_id)],
            }),
        ),
        _ => (
            format!("crowdrelay-release-{}", release_id),
            format!("{title} · release audience"),
            json!({
                "statuses": ["active"],
                "marketing_consent": true,
            }),
        ),
    };
    let segment_id=sqlx::query_scalar::<_,Uuid>(r#"INSERT INTO audience_segments(workspace_id,slug,name,description,filter,active) VALUES($1,$2,$3,'CrowdRelay release audience',$4,true) ON CONFLICT(workspace_id,slug) DO UPDATE SET active=true, filter=EXCLUDED.filter RETURNING id"#)
      .bind(workspace_id.into_uuid()).bind(&segment_slug).bind(&segment_name).bind(&segment_filter).fetch_one(&mut **tx).await.map_err(map_sqlx)?;
    let template = format!("release.{phase}.v1");
    let growth_goal = match milestone {
        crowdrelay_domain::release_autopilot::ReleaseMilestone::FanWarmup => "referral",
        crowdrelay_domain::release_autopilot::ReleaseMilestone::Wrap
        | crowdrelay_domain::release_autopilot::ReleaseMilestone::CatalogueRotation => "retention",
        _ => "engagement",
    };
    // The campaign row itself carries why its audience is narrower than the
    // release's — a reader should never have to reverse-engineer a segment.
    let audience_note = match milestone {
        crowdrelay_domain::release_autopilot::ReleaseMilestone::Wrap => {
            "second wave — only fans no earlier release phase reached"
        }
        crowdrelay_domain::release_autopilot::ReleaseMilestone::CatalogueRotation => {
            "catalogue rotation — only fans no earlier release phase reached; labelled catalogue, not new"
        }
        crowdrelay_domain::release_autopilot::ReleaseMilestone::Countdown => {
            "pre-save push — fans ranked most likely to listen from first-party edges"
        }
        _ => "release audience",
    };
    let campaign = sqlx::query_as::<_, (Uuid, String)>(
        r#"
        INSERT INTO communication_campaigns(
            workspace_id,segment_id,slug,name,channel,template_key,content
        ) VALUES(
            $1,$2,$3,$4,'email',$5,
            jsonb_build_object(
                'release_id',$6::uuid,
                'managed_by','crowdrelay',
                'growth_goal',$7::text,
                'audience_note',$8::text
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
    // communication_campaigns.name is CHECKed at 160 chars while a plan title
    // allows 240; an over-long title must not wedge every milestone send.
    .bind(format!(
        "{} · {phase}",
        title.chars().take(140).collect::<String>()
    ))
    .bind(&template)
    .bind(release_id.into_uuid())
    .bind(growth_goal)
    .bind(audience_note)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    if campaign.1 == "draft" {
        // The outbox row carries the action's trace spine — `ops/trace` shows
        // the decision → action → campaign hop, not a context-free orphan.
        let outbox_id=sqlx::query_scalar::<_,Uuid>(r#"INSERT INTO outbox_events(workspace_id,event_type,event_version,payload,available_at,trace_id,causation_id,action_id) SELECT $1,'communication.campaign_due',1,jsonb_build_object('campaign_id',$2::uuid,'campaign_slug',$3::text,'channel','email','segment_id',$4::uuid,'template_key',$5::text,'send_evidence',jsonb_build_object('source_id',$7::text,'recipient_reason',$8::text)),$6,at.trace_id,at.causation_id,at.id FROM autopilot_actions at WHERE at.id=$9 RETURNING id"#)
          .bind(workspace_id.into_uuid()).bind(campaign.0).bind(&campaign_slug).bind(segment_id).bind(&template).bind(now)
          .bind(format!("release-campaign:{campaign_slug}"))
          .bind("marketing-consent segment — every fan in it opted in")
          .bind(action_id.into_uuid())
          .fetch_one(&mut **tx).await.map_err(map_sqlx)?;
        sqlx::query("UPDATE communication_campaigns SET status='scheduled',scheduled_at=$3,dispatch_event_id=$4 WHERE workspace_id=$1 AND id=$2 AND status='draft'")
          .bind(workspace_id.into_uuid()).bind(campaign.0).bind(now).bind(outbox_id).execute(&mut **tx).await.map_err(map_sqlx)?;
    } else if !matches!(campaign.1.as_str(), "scheduled" | "completed") {
        return Err(RepositoryError::Conflict);
    }
    Ok(())
}
/// Every phase send that can precede the late waves — the exclusion set the
/// missed-it and catalogue segments subtract from the release audience. A
/// phase that never ran contributes a slug no campaign row carries, which
/// excludes nothing, so the list is declared, not computed.
fn earlier_release_campaign_slugs(release_id: crowdrelay_domain::ReleasePlanId) -> Vec<String> {
    [
        "announcement",
        "fan_warmup",
        "countdown",
        "release_day",
        "sustain",
        // The wrap is itself a late wave; the catalogue rotation behind it
        // must subtract its reach too, or R+30 re-mails everyone R+14 just
        // told about the release.
        "wrap",
    ]
    .iter()
    .map(|phase| format!("crowdrelay-release-{release_id}-{phase}"))
    .collect()
}
/// The tag the countdown write marks likely listeners with. Bounded by the
/// tag CHECK's charset — `presave-{uuid}` is 44 chars of lowercase + dashes.
fn likely_listener_tag(release_id: crowdrelay_domain::ReleasePlanId) -> String {
    format!("presave-{}", release_id.into_uuid())
}
/// 1R.5: the fans first-party edges say are most likely to listen. The
/// ranking is deliberately boring — recency of the last meaningful action
/// (joining counts as one: a fan with no edges yet is ranked by the day they
/// signed up, not dropped), then how many distinct kinds of edge they have —
/// and the same ranked rows both tag the segment and name the day-one list.
/// The cap bounds the segment; below it, "likely" is everyone it could name.
pub(in crate::autopilot) async fn tag_likely_listeners(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    release_id: crowdrelay_domain::ReleasePlanId,
    title: &str,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let tag = likely_listener_tag(release_id);
    let ranked = sqlx::query_as::<_, (Uuid, String, Option<OffsetDateTime>, i64)>(
        r#"
        WITH scored AS (
            SELECT fan.id,
                   fan.normalized_email,
                   fan_last_meaningful_action(fan.workspace_id, fan.id, fan.normalized_email)
                       AS last_action_at,
                   (
                     (SELECT count(*) FROM event_interests i
                       WHERE i.workspace_id = fan.workspace_id AND i.fan_id = fan.id)
                   + (SELECT count(*) FROM ticket_orders o
                       JOIN ticket_sales s ON s.workspace_id = o.workspace_id AND s.id = o.ticket_sale_id
                       WHERE o.workspace_id = fan.workspace_id
                         AND o.buyer_email = fan.normalized_email
                         AND o.status IN ('paid','partially_refunded','refunded'))
                   + (SELECT count(*) FROM referral_attributions r
                       WHERE r.workspace_id = fan.workspace_id AND r.referrer_fan_id = fan.id
                         AND r.status = 'qualified')
                   + (SELECT count(*) FROM synesthesia_reward_entries e
                       JOIN synesthesia_runs run ON run.workspace_id = e.workspace_id AND run.id = e.run_id
                       WHERE e.workspace_id = fan.workspace_id AND e.fan_id = fan.id
                         AND run.completed_at IS NOT NULL)
                   )::bigint AS edge_count
            FROM fans fan
            WHERE fan.workspace_id = $1
              AND fan.status = 'active'
              AND EXISTS (
                  SELECT 1 FROM fan_consents consent
                  WHERE consent.workspace_id = fan.workspace_id
                    AND consent.fan_id = fan.id
                    AND consent.purpose = 'marketing'
                    AND consent.granted
                    AND consent.id = (
                        SELECT newest.id FROM fan_consents newest
                        WHERE newest.workspace_id = consent.workspace_id
                          AND newest.fan_id = consent.fan_id
                          AND newest.purpose = consent.purpose
                        ORDER BY newest.recorded_at DESC, newest.id DESC
                        LIMIT 1
                    )
              )
        )
        SELECT id, normalized_email, last_action_at, edge_count
        FROM scored
        ORDER BY COALESCE(last_action_at, '-infinity'::timestamptz) DESC,
                 edge_count DESC, id
        LIMIT 500
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    // One statement for the whole ranked set — the tag write happens inside
    // the dispatch transaction, so five hundred round trips would hold the
    // action's locks open for nothing a set insert does in one.
    if !ranked.is_empty() {
        let fan_ids: Vec<uuid::Uuid> = ranked.iter().map(|(fan_id, _, _, _)| *fan_id).collect();
        sqlx::query(
            "INSERT INTO fan_audience_tags (workspace_id, fan_id, tag, source)
             SELECT $1, ranked.fan_id, $3, 'system'
             FROM unnest($2::uuid[]) AS ranked(fan_id)
             ON CONFLICT DO NOTHING",
        )
        .bind(workspace_id.into_uuid())
        .bind(&fan_ids)
        .bind(&tag)
        .execute(&mut **tx)
        .await
        .map_err(map_sqlx)?;
    }

    // The named list is the deliverable (§4i: "named from follow/listen
    // edges") — the band can reach the top of it personally on day one, and
    // the scoring formula travels with the artifact so nobody has to take
    // "likely" on faith.
    crate::autopilot::emit_external_action(
        tx,
        workspace_id,
        action_id,
        "crowdrelay.release.likely_listeners",
        json!({
            "action_id": action_id,
            "release_id": release_id,
            "send_evidence": crate::autopilot::send_evidence(
                format!("release:{release_id}:countdown"),
                "consented active fans tagged as likely listeners — ranking formula travels with the artifact",
            )?,
            "release": { "title": title },
            "generated_at": now,
            "tag": tag,
            "likely_listeners": ranked
                .iter()
                .take(50)
                .map(|(fan_id, email, last_action_at, edge_count)| json!({
                    "fan_id": fan_id,
                    "email": email,
                    "last_meaningful_action_at": last_action_at,
                    "edge_count": edge_count,
                }))
                .collect::<Vec<_>>(),
            "likely_listeners_total": ranked.len(),
            "ranking": "COALESCE(last_meaningful_action, signup) recency, then distinct edge count (interests + tickets + referrals + synesthesia); consented, active fans only; capped at 500",
        }),
    )
    .await?;
    Ok(())
}
