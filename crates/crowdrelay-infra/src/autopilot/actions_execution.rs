//! The execution half of the action ledger, split out of `actions.rs`.
//!
//! One method lives here — `execute_action` — and it is one `match` over every
//! payload kind in the system. Its size is the size of the payload surface;
//! splitting it further would hide which payloads exist from one read.

use super::*;

impl PostgresAutopilotRepository {
    pub(super) async fn execute_action_impl(
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
                    // The referral invite template needs the fan's active
                    // referral code so the executor can build the referral
                    // link. The code is created at signup, so it always
                    // exists by day 3 (referral_invite_after_days). Other
                    // templates leave this null — the field is additive.
                    let referral_code = if template_key == "crowdrelay.fan.referral_invite.v1" {
                        sqlx::query_scalar::<_, Option<String>>(
                            "SELECT code FROM referral_codes WHERE workspace_id=$1 AND fan_id=$2 AND active",
                        )
                        .bind(workspace_id.into_uuid())
                        .bind(fan_id.into_uuid())
                        .fetch_optional(&mut *transaction)
                        .await
                        .map_err(map_sqlx)?
                        .flatten()
                    } else {
                        None
                    };
                    emit_external_action(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "crowdrelay.fan_lifecycle.message_requested",
                        json!({
                            "action_id": action.id,
                            "fan_id": fan_id,
                            "template_key": template_key,
                            "fan": {
                                "email": fan.0,
                                "display_name": fan.1,
                                "locale": fan.2,
                                "referral_code": referral_code,
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
                        "crowdrelay.merch.reorder_requested",
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
                        "crowdrelay.booking.outreach_requested",
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
                        "crowdrelay.merch.bundle_requested",
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
                    wave_id,
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
                    // Read before the emit rather than inside it: the numbers
                    // are what the band claims, and a claim assembled halfway
                    // through building the message it travels in is harder to
                    // read than one assembled first.
                    let evidence =
                        waves::evidence_packet(&mut transaction, workspace_id, now).await?;
                    emit_external_action(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "crowdrelay.outreach.requested",
                        json!({
                            "action_id": action.id,
                            "opportunity_id": opportunity_id,
                            "target_id": target_id,
                            "target_name": target.0,
                            "contact_email": target.1,
                            "phase": phase,
                            "template_key": template_key,
                            "target_template_key": target.2,
                            "wave_id": wave_id,
                            // What was true when the band said it, rather than
                            // when the agent drafted it. No adjectives: numbers,
                            // and the moment they were read.
                            "evidence": evidence,
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
                AutopilotActionPayload::RequestOutreachDiscovery { requested_candidates } => {
                    let policy = crowdrelay_domain::target_discovery::TargetDiscoveryPolicy::default();
                    emit_external_action(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "crowdrelay.outreach.discovery_requested",
                        json!({
                            "action_id": action.id,
                            "requested_candidates": requested_candidates,                            // The adapter sweeps and reports; it never decides.
                            // These thresholds are published so a sweep can skip
                            // what would be refused on arrival anyway, and the
                            // same rules are re-applied on ingest regardless of
                            // what the adapter believed about them.
                            "callback_path": "/v1/admin/autopilot/outreach/candidates",
                            "screening_contract": {
                                "minimum_fit_basis_points": policy.minimum_fit_basis_points,
                                "minimum_follower_count": policy.minimum_follower_count,
                                "minimum_engagement_basis_points":
                                    policy.minimum_engagement_basis_points,
                                "engagement_scrutiny_follower_count":
                                    policy.engagement_scrutiny_follower_count,
                            },
                            "discovery_rules": [
                                "read_a_submission_route_only_where_it_was_published_for_that_purpose",
                                "never_infer_or_pattern_guess_an_address_from_a_name_or_domain",
                                "send_the_verbatim_published_evidence_the_route_was_read_from",
                                "record_the_source_reference_so_a_bad_source_can_be_revoked_wholesale",
                                "never_submit_through_a_channel_that_sells_placement",
                                "respect_platform_terms_and_never_fetch_what_they_forbid",
                                "a_paid_or_credit_channel_must_be_reported_as_such_not_as_free"
                            ],
                            "allowed_sources": [
                                "playlist_description", "curator_site", "submission_channel",
                                "reply", "operator_import", "scene_adjacent_playlist"
                            ],
                            "allowed_route_kinds": ["email", "submission_form", "handle"]
                        }),
                    )
                    .await?;
                }
                AutopilotActionPayload::RequestBookingTargetDiscovery { requested_count } => {
                    let policy = crowdrelay_domain::booking_discovery::BookingDiscoveryPolicy::default();
                    emit_external_action(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "crowdrelay.booking.target_discovery_requested",
                        json!({
                            "action_id": action.id,
                            "requested_count": requested_count,
                            // The adapter sweeps and reports; it never decides.
                            // Screening is re-applied on ingest regardless of
                            // what the adapter believed about the prospect.
                            "callback_path": "/v1/internal/autopilot/booking-discovery/candidates",
                            "screening_contract": {
                                "minimum_fit_basis_points": policy.minimum_fit_basis_points,
                                "require_capacity_evidence": policy.require_capacity_evidence,
                            },
                            "discovery_rules": [
                                "read_a_booking_route_only_where_it_was_published_for_that_purpose",
                                "never_infer_or_pattern_guess_an_address_from_a_name_or_domain",
                                "send_the_verbatim_published_evidence_the_route_was_read_from",
                                "record_the_source_reference_so_a_bad_source_can_be_revoked_wholesale",
                                "a_festival_asking_the_band_to_pay_to_apply_must_be_reported_as_such",
                                "city_slug_is_required_for_promotion_so_report_it_when_known"
                            ]
                        }),
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
                        "crowdrelay.beacon.discovery_requested",
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
                AutopilotActionPayload::RequestBeaconInviteBatch {
                    beacon_id,
                    beacon_version,
                    event_id,
                    requested_count,
                } => {
                    // Re-read under the same guards the rule used at decision
                    // time: hours passed since then, and a partner who went
                    // cold or a show that moved must not receive yesterday's
                    // ask. The ask travels with the facts it was built on.
                    let target = sqlx::query_as::<_, (String, String, String, String, OffsetDateTime)>(
                        r#"
                        SELECT beacon.display_name, beacon.contact_email,
                               event.title, event.slug, event.starts_at
                        FROM viryaos_beacons AS beacon
                        JOIN events AS event
                          ON event.workspace_id = beacon.workspace_id AND event.id = $3
                        WHERE beacon.workspace_id = $1 AND beacon.id = $2
                          AND beacon.version = $4
                          AND beacon.active AND beacon.verified AND beacon.accepts_outreach
                          AND NOT beacon.do_not_contact
                          AND beacon.contact_email IS NOT NULL
                          AND event.status = 'published'
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
                    // The ask travels; the codes stay here. Invite codes are
                    // issued by the workspace's own machinery when the partner
                    // answers yes, so every signup they produce is attributed
                    // and consented by construction — neither the executor
                    // nor the partner invents either.
                    emit_external_action(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "crowdrelay.beacon.invite_batch_requested",
                        json!({
                            "action_id": action.id,
                            "beacon_id": beacon_id,
                            "beacon_version": beacon_version,
                            "event_id": event_id,
                            "requested_count": requested_count,
                            "beacon_name": target.0,
                            "contact_email": target.1,
                            "event": {
                                "title": target.2,
                                "slug": target.3,
                                "starts_at": target.4,
                            },
                            "callback_path": "/v1/admin/beacons",
                            "invite_contract": {
                                "codes_issued_by_crowdrelay": true,
                                "never_purchase_or_bot_invites": true,
                                "only_their_own_community": true,
                                "one_batch_per_beacon_per_show": true
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
                        "crowdrelay.beacon.outreach_requested",
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
                    source_version: _,
                    artifact,
                    template_key,
                } => {
                    let source =
                        operations::load_content_source_for_execution(&mut transaction, workspace_id, *source_id)
                            .await?;
                    emit_external_action(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "crowdrelay.content.artifact_requested",
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
                        "crowdrelay.show.task_attention_required",
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
                        "crowdrelay.promotion.budget_change_requested",
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
                AutopilotActionPayload::EscalateEditorialPitch {
                    release_id, title, due_at,
                } => {
                    operations::escalate_editorial_pitch(
                        &mut transaction, workspace_id, action.id, *release_id, title, *due_at, now,
                    ).await?;
                }
                AutopilotActionPayload::VerifyPlaylistPlacement {
                    opportunity_id, playlist_external_id, track_external_id, checkpoint,
                } => {
                    // A public read, asked of whoever holds the credential. The
                    // result comes back through the placement ingress, so the
                    // agent never learns from its own request.
                    emit_external_action(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "crowdrelay.playlist.placement_check_requested",
                        json!({
                            "action_id": action.id,
                            "opportunity_id": opportunity_id,
                            "playlist_external_id": playlist_external_id,
                            "track_external_id": track_external_id,
                            "checkpoint": checkpoint,
                        }),
                    )
                    .await?;
                }
                AutopilotActionPayload::CounterLiveOpportunityTerms {
                    opportunity_id, ask_minor, currency, round,
                } => {
                    operations::execute_live_opportunity_terms(
                        &mut transaction, workspace_id, action.id,
                        &operations::TermsMove {
                            opportunity_id: *opportunity_id,
                            accept: false,
                            amount_minor: *ask_minor,
                            currency,
                            round: *round,
                        },
                        now,
                    ).await?;
                }
                AutopilotActionPayload::AcceptLiveOpportunityTerms {
                    opportunity_id, fee_minor, currency,
                } => {
                    operations::execute_live_opportunity_terms(
                        &mut transaction, workspace_id, action.id,
                        &operations::TermsMove {
                            opportunity_id: *opportunity_id,
                            accept: true,
                            amount_minor: *fee_minor,
                            currency,
                            round: 0,
                        },
                        now,
                    ).await?;
                }
                AutopilotActionPayload::PrepareFundingPackage { opportunity_id } => {
                    operations::prepare_funding_package(&mut transaction, workspace_id, action.id, *opportunity_id, now).await?;
                }
                AutopilotActionPayload::SubmitFundingApplication { opportunity_id } => {
                    operations::submit_funding_application(&mut transaction, workspace_id, action.id, *opportunity_id, now).await?;
                }
                AutopilotActionPayload::RunPlayStep {
                    play_id, play_kind, step_index, step_kind, event_id, fan_id, template_key,
                } => {
                    plays::execute_play_step(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        &plays::PlayStepDispatch {
                            play_id: *play_id,
                            play_kind: *play_kind,
                            step_index: *step_index,
                            step_kind: *step_kind,
                            event_id: *event_id,
                            fan_id: *fan_id,
                            template_key,
                        },
                    )
                    .await?;
                }
                AutopilotActionPayload::SendTeamAssignmentEmail {
                    assignment_id, recipient_email, recipient_name, task_title, task_detail,
                    due_at, action_url_path, reminder_number,
                } => {
                    emit_external_action(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "crowdrelay.team.assignment_email_requested",
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
                AutopilotActionPayload::RequestAgentContent {
                    template_id,
                    task_id,
                    draft,
                } => {
                    emit_external_action(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "crowdrelay.agent.content_requested",
                        json!({
                            "action_id": action.id,
                            "template_id": template_id,
                            "task_id": task_id,
                            "draft": draft,
                        }),
                    )
                    .await?;
                }
                AutopilotActionPayload::RequestOutreachTarget {
                    target_kind,
                    display_name,
                    ..
                } => {
                    // Internal DB operation: promote the outreach target from
                    // `proposed` to `promoted` in the staging table. No
                    // external executor is involved.
                    //
                    // Matched on the row's own identity -- the
                    // `(workspace_id, display_name, target_kind)` unique key the
                    // proposal conflicts on -- rather than on `source_task_id`.
                    // A re-proposal of the same target keeps the original task
                    // on the row, because that DO UPDATE does not touch
                    // `source_task_id`, while the approval action carries the
                    // task that proposed it most recently. Keyed on the task,
                    // approving a target anyone had proposed before promoted
                    // nothing at all.
                    //
                    // `promoted` is accepted so a replayed execution is a
                    // no-op rather than a conflict; `discarded` is deliberately
                    // not, because the proposal path preserves that decision
                    // and an approval must not quietly overturn it.
                    let promoted = sqlx::query(
                        r#"
                        UPDATE agent_outreach_targets
                        SET status = 'promoted', screened_at = COALESCE(screened_at, now())
                        WHERE workspace_id = $1
                          AND target_kind = $2
                          AND display_name = $3
                          AND status IN ('proposed', 'promoted')
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(target_kind)
                    .bind(display_name)
                    .execute(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                    // The sibling arms treat a write that changed nothing as a
                    // conflict rather than a success. Without this the ledger
                    // recorded the approval as executed while the target stayed
                    // `proposed`, so the growth loop never used it and nothing
                    // said why.
                    if promoted.rows_affected() != 1 {
                        return Err(RepositoryError::Conflict);
                    }
                }
                AutopilotActionPayload::RequestAgentRun {
                    template_id,
                    prompt,
                    priority,
                    tier,
                } => {
                    operations::execute_agent_run(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        template_id,
                        prompt,
                        *priority,
                        *tier,
                        now,
                    )
                    .await?;
                }
                AutopilotActionPayload::RequestCommunityEngagement {
                    target_id,
                    platform,
                    subreddit,
                    title,
                    body,
                    smart_link,
                } => {
                    emit_external_action(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "crowdrelay.community.engagement_requested",
                        json!({
                            "action_id": action.id,
                            "target_id": target_id,
                            "platform": platform,
                            "subreddit": subreddit,
                            "title": title,
                            "body": body,
                            "smart_link": smart_link,
                        }),
                    )
                    .await?;
                }
                AutopilotActionPayload::RequestSignalPush {
                    task_id: _,
                    title,
                    body,
                    target_path,
                    event_id,
                    segment,
                } => {
                    operations::execute_signal_push(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        title,
                        body,
                        target_path.as_deref(),
                        event_id.as_ref(),
                        segment.as_deref(),
                        now,
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
            // The deliverability ramp is measured from the first third-party
            // send that actually left, in the same transaction that marks it
            // left. `COALESCE` keeps the first send first, so a replayed or
            // retried completion cannot move the clock the ceiling grows on.
            if action.payload.action_class() == ActionClass::ThirdParty {
                sqlx::query(
                    r#"
                    UPDATE workspaces
                    SET first_third_party_send_at = COALESCE(first_third_party_send_at, $2)
                    WHERE id = $1
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(now)
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
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
}
