//! Split PostgreSQL Autopilot adapter implementation.

use super::*;

const TEAM_ASSIGNMENT_EMAIL_ACTION_KIND: &str = "team.assignment.email";

impl PostgresAutopilotRepository {
    pub async fn claim_due_autonomous_actions(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
        now: OffsetDateTime,
    ) -> Result<Vec<ClaimedAutopilotAction>, RepositoryError> {
        self.claim_due_actions_filtered(
            workspace_id,
            limit,
            now,
            None,
            Some(TEAM_ASSIGNMENT_EMAIL_ACTION_KIND),
        )
        .await
    }

    pub async fn claim_due_team_email_actions(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
        now: OffsetDateTime,
    ) -> Result<Vec<ClaimedAutopilotAction>, RepositoryError> {
        self.claim_due_actions_filtered(
            workspace_id,
            limit,
            now,
            Some(TEAM_ASSIGNMENT_EMAIL_ACTION_KIND),
            None,
        )
        .await
    }

    async fn claim_due_actions_filtered(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
        now: OffsetDateTime,
        include_action_kind: Option<&'static str>,
        exclude_action_kind: Option<&'static str>,
    ) -> Result<Vec<ClaimedAutopilotAction>, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            sqlx::query(
                r#"
                UPDATE viryaos_autopilot_actions
                SET status='cancelled', finished_at=$2, last_error_kind='approval_expired'
                WHERE workspace_id=$1 AND status='awaiting_approval'
                  AND ($3::text IS NULL OR action_kind = $3)
                  AND ($4::text IS NULL OR action_kind <> $4)
                  AND approval_expires_at IS NOT NULL AND approval_expires_at <= $2
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .bind(include_action_kind)
            .bind(exclude_action_kind)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            sqlx::query(
                r#"
                UPDATE viryaos_autopilot_actions
                SET status = 'failed',
                    finished_at = $2,
                    last_error_kind = 'stale_retry_exhausted'
                WHERE workspace_id = $1
                  AND status = 'processing'
                  AND ($3::text IS NULL OR action_kind = $3)
                  AND ($4::text IS NULL OR action_kind <> $4)
                  AND started_at <= $2 - INTERVAL '15 minutes'
                  AND attempt_count >= 5
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .bind(include_action_kind)
            .bind(exclude_action_kind)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            let rows = sqlx::query_as::<_, ClaimedActionRow>(
                r#"
                WITH candidates AS (
                    SELECT id
                    FROM viryaos_autopilot_actions
                    WHERE workspace_id = $1
                      AND attempt_count < 5
                      AND ($4::text IS NULL OR action_kind = $4)
                      AND ($5::text IS NULL OR action_kind <> $5)
                      AND (
                          (status = 'queued' AND available_at <= $2)
                          OR (
                              status = 'processing'
                              AND started_at <= $2 - INTERVAL '15 minutes'
                          )
                      )
                    ORDER BY available_at, id
                    FOR UPDATE SKIP LOCKED
                    LIMIT $3
                ), claimed AS (
                    UPDATE viryaos_autopilot_actions AS action
                    SET status = 'processing',
                        attempt_count = action.attempt_count + 1,
                        started_at = $2,
                        last_error_kind = NULL
                    FROM candidates
                    WHERE action.workspace_id = $1
                      AND action.id = candidates.id
                    RETURNING action.id, action.payload, action.attempt_count
                ), attempts AS (
                    INSERT INTO viryaos_autopilot_action_attempts (
                        workspace_id, action_id, attempt_number, outcome, occurred_at
                    )
                    SELECT $1, claimed.id, claimed.attempt_count, 'started', $2
                    FROM claimed
                    RETURNING action_id
                )
                SELECT claimed.id, claimed.payload, claimed.attempt_count AS attempt_number
                FROM claimed
                JOIN attempts ON attempts.action_id = claimed.id
                ORDER BY claimed.id
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .bind(i64::from(limit.min(100)))
            .bind(include_action_kind)
            .bind(exclude_action_kind)
            .fetch_all(&mut *transaction)
            .await
            .map_err(map_sqlx)?;

            let mut actions = Vec::with_capacity(rows.len());
            for row in rows {
                let payload = serde_json::from_value::<AutopilotActionPayload>(row.payload)
                    .map_err(|_| RepositoryError::Unexpected)?;
                let attempt_number =
                    u32::try_from(row.attempt_number).map_err(|_| RepositoryError::Unexpected)?;
                actions.push(ClaimedAutopilotAction {
                    id: AutopilotActionId::from_uuid(row.id),
                    payload,
                    attempt_number,
                });
            }
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(actions)
        })
        .await
    }
}

#[async_trait]
impl AutopilotActionRepository for PostgresAutopilotRepository {
    async fn claim_due_actions(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
        now: OffsetDateTime,
    ) -> Result<Vec<ClaimedAutopilotAction>, RepositoryError> {
        self.claim_due_actions_filtered(workspace_id, limit, now, None, None)
            .await
    }

    async fn execute_action(
        &self,
        workspace_id: WorkspaceId,
        action: &ClaimedAutopilotAction,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            match &action.payload {
                AutopilotActionPayload::ChangeTicketPrice {
                    ticket_type_id,
                    from_minor,
                    to_minor,
                } => {
                    execute_ticket_price_change(
                        &mut transaction,
                        workspace_id,
                        *ticket_type_id,
                        *from_minor,
                        *to_minor,
                    )
                    .await?;
                }
                AutopilotActionPayload::ChangeTicketCapacity {
                    ticket_type_id,
                    from_capacity,
                    to_capacity,
                    guardrail_version,
                } => {
                    execute_ticket_capacity_change(
                        &mut transaction,
                        workspace_id,
                        *ticket_type_id,
                        *from_capacity,
                        *to_capacity,
                        *guardrail_version,
                    )
                    .await?;
                }
                AutopilotActionPayload::RequestFanLifecycleMessage {
                    fan_id,
                    template_key,
                } => {
                    ensure_marketing_eligible(&mut transaction, workspace_id, *fan_id).await?;
                    let fan = sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
                        "SELECT normalized_email, display_name, locale FROM fans WHERE workspace_id=$1 AND id=$2 AND status='active' FOR SHARE",
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(fan_id.into_uuid())
                    .fetch_optional(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?
                    .ok_or(RepositoryError::Conflict)?;
                    emit_external_action(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "viryaos.fan_lifecycle.message_requested",
                        json!({
                            "action_id": action.id,
                            "fan_id": fan_id,
                            "template_key": template_key,
                            "fan": {
                                "email": fan.0,
                                "display_name": fan.1,
                                "locale": fan.2,
                            },
                        }),
                    )
                    .await?;
                }
                AutopilotActionPayload::RequestMerchReorder {
                    variant_id,
                    quantity,
                } => {
                    emit_external_action(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "viryaos.merch.reorder_requested",
                        json!({
                            "action_id": action.id,
                            "variant_id": variant_id,
                            "quantity": quantity,
                        }),
                    )
                    .await?;
                }
                AutopilotActionPayload::ChangeMerchPrice {
                    product_id,
                    from_minor,
                    to_minor,
                    economics_version,
                } => {
                    execute_merch_price_change(
                        &mut transaction,
                        workspace_id,
                        *product_id,
                        *from_minor,
                        *to_minor,
                        *economics_version,
                    )
                    .await?;
                }
                AutopilotActionPayload::RequestBookingOutreach {
                    city_id,
                    target_id,
                    target_version,
                    target_name: _,
                    score,
                    phase,
                } => {
                    let target = lock_booking_target_for_execution(
                        &mut transaction,
                        workspace_id,
                        *city_id,
                        *target_id,
                        *target_version,
                    )
                    .await?;
                    reserve_contact_window(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "booking_opportunity",
                        &target.2,
                        now,
                    )
                    .await?;
                    emit_external_action(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "viryaos.booking.outreach_requested",
                        json!({
                            "action_id": action.id,
                            "city_id": city_id,
                            "target_id": target_id,
                            "target_kind": target.0,
                            "target_name": target.1,
                            "contact_email": target.2,
                            "template_key": match phase { BookingOutreachPhase::Initial => "booking.opportunity.v1", BookingOutreachPhase::FollowUp => "booking.followup.v1" },
                            "phase": phase,
                            "score": score,
                        }),
                    )
                    .await?;
                    let changed = sqlx::query(
                        r#"
                        UPDATE viryaos_booking_targets
                        SET last_outreach_at = $4
                        WHERE workspace_id = $1 AND id = $2 AND version = $3
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(target_id.into_uuid())
                    .bind(target_version)
                    .bind(now)
                    .execute(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                    if changed.rows_affected() != 1 { return Err(RepositoryError::Conflict); }
                    sqlx::query(r#"
                        INSERT INTO viryaos_booking_interactions(
                            workspace_id,target_id,direction,phase,source_key,occurred_at,metadata
                        ) VALUES($1,$2,'outbound',$3,$4,$5,jsonb_build_object('action_id',$6::uuid))
                        ON CONFLICT(workspace_id,target_id,source_key) DO NOTHING
                    "#)
                    .bind(workspace_id.into_uuid())
                    .bind(target_id.into_uuid())
                    .bind(match phase { BookingOutreachPhase::Initial => "initial", BookingOutreachPhase::FollowUp => "followup" })
                    .bind(format!("autopilot:{}", action.id))
                    .bind(now)
                    .bind(action.id.into_uuid())
                    .execute(&mut *transaction).await.map_err(map_sqlx)?;
                }
                AutopilotActionPayload::RequestAudienceCampaign {
                    event_id,
                    phase,
                    template_key,
                } => {
                    operations::execute_audience_campaign(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        *event_id,
                        *phase,
                        template_key,
                        now,
                    )
                    .await?;
                }
                AutopilotActionPayload::RequestMerchBundle {
                    product_a,
                    product_b,
                    bundle_price_minor,
                    affinity_basis_points,
                } => {
                    emit_external_action(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "viryaos.merch.bundle_requested",
                        json!({
                            "action_id": action.id,
                            "product_a": product_a,
                            "product_b": product_b,
                            "bundle_price_minor": bundle_price_minor,
                            "affinity_basis_points": affinity_basis_points,
                        }),
                    )
                    .await?;
                }
                AutopilotActionPayload::RequestOutreach {
                    opportunity_id,
                    target_id,
                    target_version,
                    target_name: _,
                    phase,
                    template_key,
                } => {
                    let target = operations::lock_outreach_for_execution(
                        &mut transaction,
                        workspace_id,
                        *opportunity_id,
                        *target_id,
                        *target_version,
                    )
                    .await?;
                    reserve_contact_window(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "outreach",
                        &target.1,
                        now,
                    )
                    .await?;
                    emit_external_action(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "viryaos.outreach.requested",
                        json!({
                            "action_id": action.id,
                            "opportunity_id": opportunity_id,
                            "target_id": target_id,
                            "target_name": target.0,
                            "contact_email": target.1,
                            "phase": phase,
                            "template_key": template_key,
                            "target_template_key": target.2,
                        }),
                    )
                    .await?;
                    operations::record_outreach_sent(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        *opportunity_id,
                        *target_id,
                        *phase,
                        now,
                    )
                    .await?;
                }
                AutopilotActionPayload::RequestBeaconDiscovery { event_id, target_count } => {
                    let event = sqlx::query_as::<_, (String, Option<String>, OffsetDateTime, String, String, Option<String>)>(
                        r#"
                        SELECT event.title, event.venue, event.starts_at, city.name,
                               city.country_code, city.region
                        FROM events event
                        JOIN cities city ON city.id=event.city_id
                        WHERE event.workspace_id=$1 AND event.id=$2
                          AND event.status='published' AND event.starts_at>$3
                        FOR SHARE OF event
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(event_id.into_uuid())
                    .bind(now)
                    .fetch_optional(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?
                    .ok_or(RepositoryError::Conflict)?;
                    emit_external_action(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "viryaos.beacon.discovery_requested",
                        json!({
                            "action_id": action.id,
                            "event": {
                                "id": event_id,
                                "title": event.0,
                                "venue": event.1,
                                "starts_at": event.2,
                                "city": event.3,
                                "country_code": event.4,
                                "region": event.5,
                            },
                            "target_count": target_count,
                            "discovery_contract": {
                                "public_sources_only": true,
                                "require_source_url": true,
                                "require_verifiable_contact": true,
                                "deduplicate_before_upsert": true,
                                "allowed_kinds": [
                                    "radio", "local_press", "television", "reviewer",
                                    "creator", "photographer", "promoter", "venue", "scene_partner",
                                    "patron", "community"
                                ],
                                "priority_source_classes": [
                                    "local_metal_media_and_podcasts",
                                    "independent_radio_and_music_programmes",
                                    "venue_promoter_support_band_networks",
                                    "record_stores_rehearsal_studios_and_music_shops",
                                    "tattoo_alt_fashion_and_scene_businesses",
                                    "student_culture_portals_and_local_event_calendars",
                                    "moderated_metal_communities_and_forums",
                                    "local_live_creators_photographers_and_reviewers"
                                ],
                                "discovery_rules": [
                                    "prefer_people_and_places_with_existing_local_scene_trust_over_generic_reach",
                                    "never_treat_generic_local_businesses_as_scene_relevant_without_public_evidence",
                                    "community_candidates_must_have_public_rules_or_moderator_contact_when_available",
                                    "do_not_scrape_private_member_lists_or_personal_contact_data"
                                ],
                                "callback_path": "/v1/admin/autopilot/beacons",
                                "gemini_may_summarize_not_verify": true
                            }
                        }),
                    )
                    .await?;
                }
                AutopilotActionPayload::RequestBeaconOutreach {
                    beacon_id,
                    event_id,
                    beacon_version,
                    phase,
                    template_key,
                } => {
                    let target = sqlx::query_as::<_, (String, String, String, String, Option<String>, OffsetDateTime, String, Option<String>)>(
                        r#"
                        SELECT beacon.beacon_kind, beacon.display_name, beacon.contact_email,
                               event.title, event.venue, event.starts_at, event.slug, event.ticket_url
                        FROM viryaos_beacons AS beacon
                        JOIN events AS event
                          ON event.workspace_id = beacon.workspace_id AND event.id = $3
                        WHERE beacon.workspace_id = $1 AND beacon.id = $2
                          AND beacon.version = $4
                          AND beacon.active AND beacon.verified AND beacon.accepts_outreach
                          AND NOT beacon.do_not_contact
                          AND beacon.contact_email IS NOT NULL
                          AND event.status IN ('published','completed')
                        FOR SHARE OF beacon, event
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(beacon_id.into_uuid())
                    .bind(event_id.into_uuid())
                    .bind(beacon_version)
                    .fetch_optional(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?
                    .ok_or(RepositoryError::Conflict)?;
                    let beacon_kind = operations::parse_beacon_kind(&target.0)?;
                    let allowed_offers = beacon_kind.offer_keys_for_phase(*phase);
                    reserve_contact_window(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "beacon",
                        &target.2,
                        now,
                    )
                    .await?;
                    let city = sqlx::query_scalar::<_, Option<String>>(
                        r#"
                        SELECT city.name
                        FROM events AS event
                        LEFT JOIN cities AS city ON city.id = event.city_id
                        WHERE event.workspace_id = $1 AND event.id = $2
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(event_id.into_uuid())
                    .fetch_one(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                    let show_url = format!("https://virya.music/pl/live/{}/", target.6);
                    emit_external_action(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "viryaos.beacon.outreach_requested",
                        json!({
                            "action_id": action.id,
                            "beacon_id": beacon_id,
                            "beacon_kind": target.0,
                            "beacon_name": target.1,
                            "contact_email": target.2,
                            "event": {
                                "id": event_id,
                                "title": target.3,
                                "venue": target.4,
                                "city": city,
                                "starts_at": target.5,
                                "slug": target.6,
                                "ticket_url": target.7,
                                "show_url": show_url,
                            },
                            "phase": phase,
                            "template_key": template_key,
                            "personalization_contract": {
                                "local_reason_required": true,
                                "human_tone": true,
                                "allowed_offers": allowed_offers,
                                "epk_url": "https://virya.music/pl/epk/",
                                "single_primary_ask": true,
                                "use_event_ticket_url_when_cta_is_relevant": true,
                                "use_verified_press_or_live_proof_only": true,
                                "never_invent_local_connection_or_editorial_interest": true,
                            },
                        }),
                    )
                    .await?;
                    sqlx::query(
                        r#"
                        INSERT INTO viryaos_beacon_campaigns (
                            workspace_id, beacon_id, event_id, status, last_phase,
                            last_outreach_at, followup_count
                        ) VALUES ($1,$2,$3,'contacted',$4,$5,1)
                        ON CONFLICT (workspace_id, beacon_id, event_id) DO UPDATE
                        SET status = CASE
                                WHEN viryaos_beacon_campaigns.status IN ('interested','partner')
                                THEN viryaos_beacon_campaigns.status
                                ELSE 'contacted'
                            END,
                            last_phase = EXCLUDED.last_phase,
                            last_outreach_at = EXCLUDED.last_outreach_at,
                            followup_count = viryaos_beacon_campaigns.followup_count + 1
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(beacon_id.into_uuid())
                    .bind(event_id.into_uuid())
                    .bind(match phase {
                        crowdrelay_domain::beacons::BeaconOutreachPhase::Initial => "initial",
                        crowdrelay_domain::beacons::BeaconOutreachPhase::CollaborationFollowUp => "collaboration_follow_up",
                        crowdrelay_domain::beacons::BeaconOutreachPhase::LocalPush => "local_push",
                        crowdrelay_domain::beacons::BeaconOutreachPhase::PostShowThanks => "post_show_thanks",
                    })
                    .bind(now)
                    .execute(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                }
                AutopilotActionPayload::RaiseGrowthOpportunity { .. } => {
                    // Deliberately no side effect. The finding is the work: the
                    // durable action row carries the evidence, the recommended
                    // class of response and the authority it was raised under,
                    // and the operator queue reads it from there. Emitting a
                    // provider call would mean assuming a capability the
                    // platform was never declared to have, and inventing a
                    // first-party mutation would fabricate state the evidence
                    // does not support.
                }
                AutopilotActionPayload::IssueReferralCode { fan_id } => {
                    // Same shape as the existing self-service path in
                    // `fan_lifecycle`: one code per fan, and a second one would
                    // split their referrals across two identities and make the
                    // ledger wrong. The insert is guarded rather than blind so a
                    // replay is a no-op instead of a duplicate.
                    sqlx::query(
                        r#"
                        INSERT INTO referral_codes (workspace_id, fan_id, code)
                        SELECT $1, $2, encode(gen_random_bytes(18), 'hex')
                        WHERE NOT EXISTS (
                            SELECT 1 FROM referral_codes
                            WHERE workspace_id = $1 AND fan_id = $2 AND active
                        )
                        ON CONFLICT DO NOTHING
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(fan_id.into_uuid())
                    .execute(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                }
                AutopilotActionPayload::RaiseGrowthDebt { .. } => {
                    // Deliberately no side effect, for the same reason as the
                    // raised growth opportunity: the finding is the work. What
                    // to do about neglected work is an operator's call, and
                    // auto-sending an outreach message here would move paid,
                    // outward-facing work behind an observation quota.
                }
                AutopilotActionPayload::RequestShowGrowth {
                    event_id,
                    lever,
                    template_key,
                } => {
                    operations::execute_show_growth(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        *event_id,
                        *lever,
                        template_key,
                        now,
                    )
                    .await?;
                }
                AutopilotActionPayload::RequestContentArtifact {
                    source_id,
                    source_version,
                    artifact,
                    template_key,
                } => {
                    let source = operations::load_content_source_for_execution(
                        &mut transaction,
                        workspace_id,
                        *source_id,
                        *source_version,
                    )
                    .await?;
                    emit_external_action(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "viryaos.content.artifact_requested",
                        json!({
                            "action_id": action.id,
                            "source_id": source_id,
                            "source_kind": source.0,
                            "source_title": source.1,
                            "source_metadata": source.2,
                            "artifact": artifact,
                            "template_key": template_key,
                        }),
                    )
                    .await?;
                }
                AutopilotActionPayload::AdjustExperiment {
                    experiment_id,
                    expected_version,
                    winner_variant_id,
                    allocations,
                    complete,
                } => {
                    operations::execute_experiment_adjustment(
                        &mut transaction,
                        workspace_id,
                        *experiment_id,
                        *expected_version,
                        *winner_variant_id,
                        allocations,
                        *complete,
                    )
                    .await?;
                }
                AutopilotActionPayload::CompleteShowTask { event_id, task } => {
                    operations::complete_show_task(
                        &mut transaction,
                        workspace_id,
                        *event_id,
                        *task,
                        now,
                    )
                    .await?;
                }
                AutopilotActionPayload::EscalateShowTask { event_id, task } => {
                    emit_external_action(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "viryaos.show.task_attention_required",
                        json!({
                            "action_id": action.id,
                            "event_id": event_id,
                            "task": task,
                        }),
                    )
                    .await?;
                }
                AutopilotActionPayload::RequestPromotionBudgetChange {
                    campaign_id,
                    from_minor,
                    to_minor,
                    roas_basis_points,
                } => {
                    ensure_promotion_state_current(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        *campaign_id,
                        *from_minor,
                        *to_minor,
                    )
                    .await?;
                    emit_external_action(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "viryaos.promotion.budget_change_requested",
                        json!({
                            "action_id": action.id,
                            "campaign_id": campaign_id,
                            "from_minor": from_minor,
                            "to_minor": to_minor,
                            "roas_basis_points": roas_basis_points,
                        }),
                    )
                    .await?;
                }
                AutopilotActionPayload::ExecuteReleaseMilestone { release_id, title, release_at, milestone } => {
                    operations::execute_release_milestone(&mut transaction, workspace_id, action.id, *release_id, title, *release_at, *milestone, now).await?;
                }
                AutopilotActionPayload::ApplyLiveOpportunity { opportunity_id, opportunity_kind, score } => {
                    operations::execute_live_opportunity(&mut transaction, workspace_id, action.id, *opportunity_id, *opportunity_kind, *score, now).await?;
                }
                AutopilotActionPayload::PrepareFundingPackage { opportunity_id } => {
                    operations::prepare_funding_package(&mut transaction, workspace_id, action.id, *opportunity_id, now).await?;
                }
                AutopilotActionPayload::SubmitFundingApplication { opportunity_id } => {
                    operations::submit_funding_application(&mut transaction, workspace_id, action.id, *opportunity_id, now).await?;
                }
                AutopilotActionPayload::SendTeamAssignmentEmail {
                    assignment_id, recipient_email, recipient_name, task_title, task_detail,
                    due_at, action_url_path, reminder_number,
                } => {
                    emit_external_action(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "viryaos.team.assignment_email_requested",
                        json!({
                            "action_id": action.id,
                            "assignment_id": assignment_id,
                            "recipient_email": recipient_email,
                            "recipient_name": recipient_name,
                            "task_title": task_title,
                            "task_detail": task_detail,
                            "due_at": due_at,
                            "action_url_path": action_url_path,
                            "reminder_number": reminder_number,
                        }),
                    )
                    .await?;
                }
            }

            // External intents are only *dispatched* here. Their learning/outcome
            // evidence is committed when the executor reports provider-confirmed
            // success, so a queued webhook can never masquerade as completed work.
            if !payload_requires_executor(&action.payload) {
                schedule_effect_measurement(
                    &mut transaction,
                    workspace_id,
                    action.id,
                    &action.payload,
                    now,
                )
                .await?;

                record_execution_outcome(
                    &mut transaction,
                    workspace_id,
                    action.id,
                    &action.payload,
                    now,
                )
                .await?;
            }

            let completed = sqlx::query(
                r#"
                UPDATE viryaos_autopilot_actions
                SET status = 'succeeded', finished_at = $3, last_error_kind = NULL
                WHERE workspace_id = $1 AND id = $2 AND status = 'processing'
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(action.id.into_uuid())
            .bind(now)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            if completed.rows_affected() != 1 {
                return Err(RepositoryError::Conflict);
            }
            sqlx::query(
                r#"
                INSERT INTO viryaos_autopilot_action_attempts (
                    workspace_id, action_id, attempt_number, outcome, occurred_at
                ) VALUES ($1,$2,$3,'succeeded',$4)
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(action.id.into_uuid())
            .bind(i32::try_from(action.attempt_number).map_err(|_| RepositoryError::Unexpected)?)
            .bind(now)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(())
        })
        .await
    }

    async fn fail_action(
        &self,
        workspace_id: WorkspaceId,
        action_id: AutopilotActionId,
        error_kind: &'static str,
        retryable: bool,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let attempt = sqlx::query_scalar::<_, i32>(
                r#"
                UPDATE viryaos_autopilot_actions
                SET status = CASE
                        WHEN $5 AND attempt_count < 5 THEN 'queued'
                        ELSE 'failed'
                    END,
                    available_at = CASE
                        WHEN $5 AND attempt_count < 5 THEN $3 + INTERVAL '5 minutes'
                        ELSE available_at
                    END,
                    started_at = CASE
                        WHEN $5 AND attempt_count < 5 THEN NULL
                        ELSE started_at
                    END,
                    finished_at = CASE
                        WHEN $5 AND attempt_count < 5 THEN NULL
                        ELSE $3
                    END,
                    last_error_kind = $4
                WHERE workspace_id = $1 AND id = $2 AND status = 'processing'
                RETURNING attempt_count
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(action_id.into_uuid())
            .bind(now)
            .bind(error_kind)
            .bind(retryable)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            if let Some(attempt) = attempt {
                sqlx::query(
                    r#"
                    INSERT INTO viryaos_autopilot_action_attempts (
                        workspace_id, action_id, attempt_number, outcome, error_kind, occurred_at
                    ) VALUES ($1,$2,$3,'failed',$4,$5)
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(action_id.into_uuid())
                .bind(attempt)
                .bind(error_kind)
                .bind(now)
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
            }
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(())
        })
        .await
    }
}
