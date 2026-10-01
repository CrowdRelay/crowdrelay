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
                    let wordmark = sqlx::query_scalar::<_, String>(
                        "SELECT crowdrelay_workspace_wordmark($1)",
                    )
                    .bind(workspace_id.into_uuid())
                    .fetch_one(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                    // The referral invite is the fan→fan growth loop: the
                    // executor receives a complete first-party URL rather
                    // than reconstructing one from the code, and a missing
                    // code or site is terminal for this message.
                    let (referral_code, referral_url) =
                        if template_key == "crowdrelay.fan.referral_invite.v1" {
                            let code = sqlx::query_scalar::<_, Option<String>>(
                                "SELECT code FROM referral_codes WHERE workspace_id=$1 AND fan_id=$2 AND active",
                            )
                            .bind(workspace_id.into_uuid())
                            .bind(fan_id.into_uuid())
                            .fetch_optional(&mut *transaction)
                            .await
                            .map_err(map_sqlx)?
                            .flatten()
                            .ok_or(RepositoryError::ConflictBecause(
                                "referral invite refused: fan has no active referral code",
                            ))?;
                            let brand = crate::tenant_settings::TenantSettingsRepository::new(
                                self.pool.clone(),
                            )
                            .brand_settings(workspace_id.into_uuid())
                            .await
                            .map_err(map_sqlx)?;
                            let url = brand.referral_url(&code).ok_or(
                                RepositoryError::ConflictBecause(
                                    "referral invite refused: tenant has no member site URL",
                                ),
                            )?;
                            (Some(code), Some(url))
                        } else {
                            (None, None)
                        };
                    emit_outward_action(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "crowdrelay.fan_lifecycle.message_requested",
                        format!("fan-lifecycle:{fan_id}"),
                        format!(
                            "consented fan, lifecycle template {template_key} — marketing consent is re-checked at send"
                        ),
                        json!({
                            "action_id": action.id,
                            "fan_id": fan_id,
                            "template_key": template_key,
                            "brand": {
                                "wordmark": wordmark,
                            },
                            "fan": {
                                "email": fan.0,
                                "display_name": fan.1,
                                "locale": fan.2,
                                "referral_code": referral_code,
                                "referral_url": referral_url,
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
                // §12-6: the anchor plus its same-city recipients are one
                // letter — every recipient is locked and reserved inside this
                // transaction, and the first failure rolls back all of them.
                AutopilotActionPayload::RequestBookingOutreach { .. } => {
                    operations::execute_booking_outreach(
                        &mut transaction,
                        workspace_id,
                        action,
                        now,
                    )
                    .await?;
                }
                // §12-6, 4G.4: one letter to everybody who books the room, or
                // none of it — the reservations are this transaction's.
                AutopilotActionPayload::RequestGigOutreach { .. } => {
                    operations::execute_gig_outreach(&mut transaction, workspace_id, action, now)
                        .await?;
                }
                AutopilotActionPayload::RequestAudienceCampaign {
                    event_id,
                    phase,
                    template_key,
                    // The audience facts are for the approval screen, not for
                    // the send: the segment is rebuilt from the filter here, so
                    // a count taken at decision time must never be what the
                    // campaign is actually addressed to.
                    audience_size: _,
                    audience_basis: _,
                    draft,
                } => {
                    operations::execute_audience_campaign(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        operations::AudienceCampaignOrder {
                            event_id: *event_id,
                            phase: *phase,
                            template_key,
                            draft,
                        },
                        now,
                    )
                    .await?;
                }
                AutopilotActionPayload::RequestSourceCampaign {
                    source_id,
                    template_key,
                    audience_size: _,
                    audience_basis: _,
                    draft,
                } => {
                    // The drop surge's email leg — same campaign machinery as
                    // the event-bound sibling, anchored on the content source
                    // instead of a show. The tracked link is minted first so
                    // the copy's URL resolves the moment the mailer sends.
                    operations::drop_surge::ensure_drop_surge_link(
                        &mut transaction,
                        workspace_id,
                        *source_id,
                        "email",
                    )
                    .await?;
                    operations::execute_source_campaign(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        operations::SourceCampaignOrder {
                            source_id: *source_id,
                            template_key,
                            draft,
                        },
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
                    draft,
                } => {
                    // The letter was composed when the action was written. A
                    // row queued before then — or one whose draft was lost —
                    // is refused here rather than passed to an executor that
                    // would write on the band's behalf.
                    if draft.subject.trim().is_empty() || draft.body.trim().is_empty() {
                        return Err(RepositoryError::ConflictBecause(
                            "outreach refused: this action carries no letter — the pitch composes when the action is written",
                        ));
                    }
                    let target = operations::lock_outreach_for_execution(
                        &mut transaction,
                        workspace_id,
                        *opportunity_id,
                        *target_id,
                        *target_version,
                    )
                    .await?;
                    // A show opportunity's letter must be the show letter —
                    // until #322 every one composed the catalogue pitch
                    // instead. Fresh show letters carry the opportunity's own
                    // template key; anything else under an `event.*`
                    // opportunity is a stale draft and is refused, never
                    // re-composed here.
                    if crate::autopilot::outreach_supply::stale_show_letter(&target.2, template_key) {
                        return Err(RepositoryError::ConflictBecause(
                            "outreach refused: this show's letter was composed as an album pitch before show letters existed — it names no show",
                        ));
                    }
                    reserve_contact_window(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "outreach",
                        &target.1,
                        now,
                        false,
                    )
                    .await?;
                    // Read before the emit rather than inside it: the numbers
                    // are what the band claims, and a claim assembled halfway
                    // through building the message it travels in is harder to
                    // read than one assembled first.
                    let evidence =
                        waves::evidence_packet(&mut transaction, workspace_id, now).await?;
                    emit_outward_action(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "crowdrelay.outreach.requested",
                        format!("outreach-target:{target_id}"),
                        format!(
                            "verified outreach target, accepts outreach, not suppressed — locked at version {target_version}"
                        ),
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
                            "draft": draft,
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
                AutopilotActionPayload::RequestOutreachReply { .. } => {
                    operations::execute_outreach_reply(
                        &mut transaction,
                        workspace_id,
                        action,
                        now,
                    )
                    .await?;
                }
                AutopilotActionPayload::RequestRepresentationApproach {
                    target_id,
                    target_version,
                    target_name: _,
                    note,
                    draw_evidence: _,
                    draft,
                } => {
                    // The letter was composed at request time. A row queued
                    // before then — or one whose draft was lost — is refused
                    // here rather than passed to an executor that would write
                    // on the band's behalf. Approve again to compose it.
                    if draft.subject.trim().is_empty() || draft.body.trim().is_empty() {
                        return Err(RepositoryError::ConflictBecause(
                            "representation approach refused: this action carries no letter — \
                             approve the approach again to compose it",
                        ));
                    }
                    let target = crate::representation::lock_representation_for_execution(
                        &mut transaction,
                        workspace_id,
                        *target_id,
                        *target_version,
                        now,
                    )
                    .await?;
                    reserve_contact_window(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "representation",
                        &target.contact_email,
                        now,
                        false,
                    )
                    .await?;
                    // The listing is the pitch: the lock just proved it is
                    // published, and the payload is what an admitted reader
                    // would see — the domain-redacted claims plus the token
                    // the share link is built from. The band's note rides
                    // along when it gave one.
                    let listing = crate::band_listing::PostgresBandListingRepository::new(
                        self.pool.clone(),
                    )
                    .load_state(workspace_id.into_uuid())
                    .await
                    .map_err(|_| RepositoryError::Unexpected)?
                    .ok_or(RepositoryError::Conflict)?;
                    let redacted = crowdrelay_domain::listing::redact(&listing.listing)
                        .ok_or(RepositoryError::Conflict)?;
                    emit_outward_action(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "crowdrelay.representation.approach_requested",
                        format!("representation-target:{target_id}"),
                        "screened representation target — locked under version, published listing attached",
                        json!({
                            "action_id": action.id,
                            "target_id": target_id,
                            "target_name": target.display_name,
                            "target_kind": target.target_kind,
                            "contact_email": target.contact_email,
                            "note": note,
                            "share_token": listing.share_token,
                            "listing": redacted,
                            // The letter itself, approved word for word —
                            // the executor sends `draft.body` verbatim and
                            // refuses the payload without it.
                            "draft": draft,
                            // The pitch is the numbers — the lock just
                            // re-measured them inside the gate, so the letter
                            // cites the figure that cleared it, never the
                            // figure the request remembered.
                            "draw_evidence": target.draw_evidence,
                        }),
                    )
                    .await?;
                    crate::representation::record_approach_sent(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        *target_id,
                        now,
                    )
                    .await?;
                }
                AutopilotActionPayload::RequestBookingAgentApproach {
                    agent_id,
                    agent_version,
                    agent_name: _,
                    agency: _,
                    note,
                    evidence: _,
                    draft,
                } => {
                    // The letter was composed at request time. A row queued
                    // before then — or one whose draft was lost — is refused
                    // here rather than passed to an executor that would write
                    // on the band's behalf. Approve again to compose it.
                    if draft.subject.trim().is_empty() || draft.body.trim().is_empty() {
                        return Err(RepositoryError::ConflictBecause(
                            "booking-agent approach refused: this action carries no letter — \
                             approve the approach again to compose it",
                        ));
                    }
                    // The lock re-runs every request-time gate — standing,
                    // the season door, the season spend, and the draw floor
                    // re-measured now — so an approval that went stale cannot
                    // send. The evidence it hands back is the figure that
                    // cleared the gate, which is what the letter cites.
                    let agent = crate::booking_agents::lock_agent_for_execution(
                        &mut transaction,
                        workspace_id,
                        *agent_id,
                        *agent_version,
                        now,
                    )
                    .await?;
                    reserve_contact_window(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "booking_agent",
                        &agent.contact_email,
                        now,
                        false,
                    )
                    .await?;
                    emit_outward_action(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "crowdrelay.booking_agent.approach_requested",
                        format!("booking-agent:{agent_id}"),
                        "screened booking agent — locked under version, draw evidence re-measured at dispatch",
                        json!({
                            "action_id": action.id,
                            "agent_id": agent_id,
                            "agent_name": agent.name,
                            "agency": agent.agency,
                            "contact_email": agent.contact_email,
                            "note": note,
                            "evidence": agent.evidence,
                            // The letter itself, approved word for word —
                            // the executor sends `draft.body` verbatim and
                            // refuses the payload without it.
                            "draft": draft,
                        }),
                    )
                    .await?;
                    crate::booking_agents::record_approach_sent(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        *agent_id,
                        now,
                    )
                    .await?;
                }
                AutopilotActionPayload::RequestBookingAgentReply { .. } => {
                    operations::execute_booking_agent_reply(
                        &mut transaction,
                        workspace_id,
                        action,
                        now,
                    )
                    .await?;
                }
                // Every approach the approval covered re-runs its own gates
                // inside the wave's transaction — one moved gate fails the
                // wave rather than sending the rest of a batch the approval
                // never priced separately.
                AutopilotActionPayload::RequestBookingAgentApproachWave { .. } => {
                    operations::execute_booking_agent_approach_wave(
                        &mut transaction,
                        workspace_id,
                        action,
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
                    execute_beacon_discovery(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        *event_id,
                        *target_count,
                        now,
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
                        FROM beacons AS beacon
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
                    emit_outward_action(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "crowdrelay.beacon.invite_batch_requested",
                        format!("beacon:{beacon_id}"),
                        "verified beacon partner, accepts outreach — invite batch for its community",
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
                                "starts_at": crowdrelay_domain::wire_time::Wire(&target.4),
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
                AutopilotActionPayload::RequestLatarnikInvite {
                    beacon_id,
                    beacon_version,
                    recipient_email,
                    recipient_name,
                    reason,
                    draft,
                } => {
                    execute_latarnik_invite(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        *beacon_id,
                        *beacon_version,
                        recipient_email,
                        recipient_name,
                        reason,
                        draft,
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
                    execute_beacon_outreach(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        *beacon_id,
                        *event_id,
                        *beacon_version,
                        phase,
                        template_key,
                        now,
                    )
                    .await?;
                }
                AutopilotActionPayload::RaiseGrowthOpportunity { .. } => {
                    // Deliberately no side effect: the finding is the work,
                    // the durable action row carries the evidence and the
                    // operator queue reads it from there. A provider call
                    // would assume a capability the platform never declared,
                    // and a first-party mutation would fabricate state the
                    // evidence does not support.
                }
                AutopilotActionPayload::RaiseDeclineAdvisory {
                    place_id,
                    subreddit,
                    window_days,
                    ..
                } => {
                    // Approving is the park: the place flips to `not_a_fit`,
                    // the same switch the console's block control uses, so
                    // `load_community_targets` drops the room next cycle.
                    // Flipping back un-parks it — reversible by design.
                    if let Some(place_id) = place_id {
                        sqlx::query(
                            r#"UPDATE discovery_places
                               SET membership_state = 'not_a_fit',
                                   membership_changed_at = now(),
                                   membership_changed_by = 'autopilot:decline-advisory',
                                   membership_note = $3,
                                   updated_at = now()
                               WHERE id = $2 AND workspace_id = $1"#,
                        )
                        .bind(workspace_id.into_uuid())
                        .bind(place_id)
                        .bind(format!(
                            "{subreddit} engaged but produced zero fan conversions in {window_days} days — parked on approval"
                        ))
                        .execute(&mut *transaction)
                        .await
                        .map_err(map_sqlx)?;
                    }
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
                    // raised growth opportunity: the finding is the work, and
                    // auto-sending outreach here would move paid work behind
                    // an observation quota.
                }
                AutopilotActionPayload::RunArchivePromoteWave {
                    limit, reason, ..
                } => {
                    // The same mechanism the operator endpoint calls — the
                    // safeguards cannot drift apart between launch paths.
                    operations::run_archive_promote_wave(
                        &mut transaction,
                        &self.pool,
                        workspace_id,
                        *limit,
                        reason.clone(),
                    )
                    .await?;
                }
                AutopilotActionPayload::RaiseContentSuggestion { suggestion_id, .. } => {
                    operations::approve_content_suggestion(&mut transaction, workspace_id, *suggestion_id)
                        .await?;
                }
                AutopilotActionPayload::RaiseContentArc { arc_id, .. } => {
                    operations::approve_content_arc(&mut transaction, workspace_id, action.id, *arc_id)
                        .await?;
                }
                AutopilotActionPayload::RequestShowGrowth {
                    event_id,
                    lever,
                    template_key,
                    send_at,
                } => {
                    operations::execute_show_growth(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        *event_id,
                        *lever,
                        template_key,
                        *send_at,
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
                    operations::execute_content_artifact(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        *source_id,
                        *artifact,
                        template_key,
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
                    match *task {
                        // The report task resolves into the report itself —
                        // labelled numbers to band and counterparty — not into
                        // another reminder to go write one.
                        crowdrelay_domain::show_operations::ShowTaskKind::PostShowReport => {
                            operations::issue_post_show_report(
                                &mut transaction,
                                workspace_id,
                                action.id,
                                *event_id,
                                now,
                            )
                            .await?;
                        }
                        task => {
                            // Reconciliation escalating means the night is over
                            // even when no post-show lever ever qualified (an
                            // empty room, communication off): the show still
                            // becomes harvestable material so the night is not
                            // lost to the supply chain.
                            if task
                                == crowdrelay_domain::show_operations::ShowTaskKind::PostShowReconciliation
                            {
                                operations::ensure_show_completed_source(
                                    &mut transaction,
                                    workspace_id,
                                    *event_id,
                                )
                                .await?;
                            }
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
                    }
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
                AutopilotActionPayload::ApplyLiveOpportunity { opportunity_id, opportunity_kind, score, draft } => {
                    // The application letter was composed when the action was
                    // written. A row queued before then — or one whose draft
                    // was lost — is refused here rather than passed to an
                    // executor that would write on the band's behalf.
                    if draft.subject.trim().is_empty() || draft.body.trim().is_empty() {
                        return Err(RepositoryError::ConflictBecause(
                            "application refused: this action carries no letter — the application composes when the action is written",
                        ));
                    }
                    operations::execute_live_opportunity(&mut transaction, workspace_id, action.id, &operations::ApplyRequest {
                        opportunity_id: *opportunity_id, kind: *opportunity_kind, score: *score, draft,
                    }, now).await?;
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
                AutopilotActionPayload::IssueCounterpartyReport { opportunity_id, event_id } => {
                    operations::issue_counterparty_report(
                        &mut transaction, workspace_id, action.id, *opportunity_id, *event_id, now,
                    )
                    .await?;
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
                AutopilotActionPayload::SetEventTicketUrl {
                    event_id,
                    ticket_url,
                    ..
                } => {
                    // The apply re-verifies what the sweep verified — the gap
                    // is still a gap and the sale is still open — inside this
                    // action's own attempt, so an approved stale proposal
                    // cannot write.
                    plays::apply_event_ticket_url(
                        &mut transaction,
                        workspace_id,
                        *event_id,
                        ticket_url,
                    )
                    .await?;
                }
                AutopilotActionPayload::SendTeamAssignmentEmail {
                    assignment_id, recipient_email, recipient_name, task_title, task_detail,
                    due_at, action_url_path, reminder_number, informational,
                    approve_url, skip_url, pending_approvals,
                } => {
                    // The frame the executor wraps around the task body is
                    // composed here, in the same transaction and the same
                    // locale the body was written in — the alternative is a
                    // second copy of the wording living inside n8n.
                    let locale =
                        super::team::crew_locale_in_tx(&mut transaction, workspace_id).await;
                    let (email_subject, email_greeting, email_intro) =
                        super::team::team_email_frame(
                            locale,
                            recipient_name,
                            task_title,
                            *reminder_number,
                            *informational,
                        );
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
                            "due_at": crowdrelay_domain::wire_time::Wire(&due_at),
                            "action_url_path": action_url_path,
                            "reminder_number": reminder_number,
                            "approve_url": approve_url,
                            "skip_url": skip_url,
                            "pending_approvals": pending_approvals,
                            "locale": locale.as_str(),
                            "email_subject": email_subject,
                            "email_greeting": email_greeting,
                            "email_intro": email_intro,
                        }),
                    )
                    .await?;
                }
                AutopilotActionPayload::RequestAgentContent {
                    template_id,
                    task_id,
                    draft,
                    recipient_email,
                    recipient_name,
                    recipient_target_id,
                } => {
                    // The recipient travels with the event: a press pitch is
                    // an email and this event is the only thing the mailer
                    // sees. The payload-frozen address is not trusted on its
                    // own — a registry target is re-pinned here under the lock
                    // because a contact can be marked do-not-contact,
                    // deactivated, or re-addressed between the click and the
                    // claim, and a row carrying a different address is a
                    // different recipient than the operator approved. The
                    // contact window is reserved in the same transaction, so
                    // the pitch spends against the same cooldown and org
                    // budget every other outbound send answers to.
                    // A drop-surge draft is composed by the brain, not a
                    // model — `draft.drop_surge` marks it, and execution
                    // owes it two things the model path gets for free: the
                    // completed task row the channel executors join on
                    // (`task_id` is derived per lane, not the id of a task
                    // that ran), and the tracked `/l/` link its `cta_url`
                    // names, minted before the action can succeed so the
                    // executor never materializes a post bound to nothing.
                    operations::drop_surge::materialize_surge_draft(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        *task_id,
                        template_id.as_deref(),
                        draft,
                    )
                    .await?;
                    let (send_email, send_name) = if let Some(target_id) = recipient_target_id
                    {
                        let pinned = sqlx::query_as::<_, (String, String)>(
                            r#"
                            SELECT target.display_name, target.contact_email
                            FROM outreach_targets AS target
                            WHERE target.workspace_id = $1
                              AND target.id = $2
                              AND target.active
                              AND target.accepts_outreach
                              AND NOT target.do_not_contact
                              AND target.contact_email IS NOT NULL
                              AND btrim(target.contact_email) <> ''
                            FOR SHARE OF target
                            "#,
                        )
                        .bind(workspace_id.into_uuid())
                        .bind(*target_id)
                        .fetch_optional(&mut *transaction)
                        .await
                        .map_err(map_sqlx)?
                        .ok_or(RepositoryError::Conflict)?;
                        if recipient_email.as_deref().map(|mail| mail.trim().to_lowercase())
                            != Some(pinned.1.trim().to_lowercase())
                        {
                            return Err(RepositoryError::Conflict);
                        }
                        reserve_contact_window(
                            &mut transaction,
                            workspace_id,
                            action.id,
                            "press_pitch",
                            &pinned.1,
                            now,
                            false,
                        )
                        .await?;
                        (Some(pinned.1), Some(pinned.0))
                    } else {
                        (recipient_email.clone(), recipient_name.clone())
                    };
                    let event = json!({
                        "action_id": action.id,
                        "template_id": template_id,
                        "task_id": task_id,
                        "draft": draft,
                        "recipient_email": send_email,
                        "recipient_name": send_name,
                        "recipient_target_id": recipient_target_id,
                    });
                    // A draft with an address is an outward send — the payload
                    // classes itself third-party for exactly this shape, so the
                    // gate will demand the evidence a channel draft can omit.
                    if send_email.is_some() || recipient_target_id.is_some() {
                        emit_outward_action(
                            &mut transaction,
                            workspace_id,
                            action.id,
                            "crowdrelay.agent.content_requested",
                            format!("agent-task:{task_id}"),
                            "agent-drafted pitch to a recipient the operator approved — re-pinned and contact window reserved at dispatch",
                            event,
                        )
                        .await?;
                    } else {
                        emit_external_action(
                            &mut transaction,
                            workspace_id,
                            action.id,
                            "crowdrelay.agent.content_requested",
                            event,
                        )
                        .await?;
                    }
                }
                AutopilotActionPayload::RequestOutreachTarget {
                    target_kind,
                    display_name,
                    ..
                } => {
                    // Internal DB operation: promote the outreach target from
                    // `proposed` to `promoted` in the staging table. Matched
                    // on the row's own `(workspace_id, display_name,
                    // target_kind)` key rather than `source_task_id` — a
                    // re-proposal keeps the original task on the row, so
                    // task-keyed matching promoted nothing. `promoted` is
                    // accepted so a replayed execution is a no-op rather than
                    // a conflict; `discarded` is deliberately not, because the
                    // proposal path preserves that decision and an approval
                    // must not quietly overturn it.
                    let promoted = sqlx::query(
                        r#"
                        UPDATE agent_outreach_targets
                        SET status = 'promoted',
                            screened_at = COALESCE(screened_at, now()),
                            -- Promotion means the operator confirmed a real
                            -- published route; for these kinds a published
                            -- pitch route is the consent. Rows without an
                            -- address stay consented-out — a name alone is a
                            -- lead, not a recipient.
                            accepts_outreach = accepts_outreach
                                OR (contact_email IS NOT NULL AND btrim(contact_email) <> '')
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
                    image_url,
                    media_id,
                    source_url,
                    source_id,
                    creative_family: _,
                } => {
                    // The language gate once more before the post leaves:
                    // see `refuse_draft_in_wrong_language`.
                    super::lapsed_sweep::refuse_draft_in_wrong_language(
                        &mut transaction,
                        workspace_id,
                        &target_id.to_string(),
                        &format!("{title}\n{body}"),
                    )
                    .await?;
                    emit_outward_action(
                        &mut transaction,
                        workspace_id,
                        action.id,
                        "crowdrelay.community.engagement_requested",
                        format!("community-target:{target_id}"),
                        format!("community screened and admitted — {platform}/{subreddit:?}"),
                        json!({
                            "action_id": action.id,
                            "target_id": target_id,
                            "platform": platform,
                            "subreddit": subreddit,
                            "title": title,
                            "body": body,
                            "smart_link": smart_link,
                            "image_url": image_url,
                            "media_id": media_id,
                            "source_url": source_url,
                            "source_id": source_id,
                        }),
                    )
                    .await?;
                }
                AutopilotActionPayload::RequestSignalPush {
                    task_id,
                    title,
                    body,
                    target_path,
                    event_id,
                    segment,
                    audience_size: _,
                    audience_basis: _,
                    drop_surge_lane,
                } => {
                    // A surge push's `task_id` is the source it promotes and
                    // its `target_path` names a `/l/` slug — the link row
                    // must exist before the push lands or the tap resolves
                    // to nowhere.
                    if let Some(lane) = drop_surge_lane {
                        operations::drop_surge::ensure_drop_surge_link(
                            &mut transaction,
                            workspace_id,
                            ContentSourceId::from_uuid(*task_id),
                            lane,
                        )
                        .await?;
                    }
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
                AutopilotActionPayload::PublishJoinAsk { .. } => {
                    // Deliberately no side effect: the payload on the
                    // `succeeded` action row IS the instruction. The social
                    // post executor claims these actions by kind and files
                    // the `social_posts` row that is the publish receipt —
                    // the same claim shape the channel executors use for
                    // agent drafts, minus the outbox hop nobody consumes.
                }
            }

            // External intents are only *dispatched* here. Their learning/outcome
            // evidence is committed when the executor reports provider-confirmed
            // success, so a queued webhook can never masquerade as completed work.
            if !payload_requires_executor(&action.payload) {
                // The envelope first: outcome-created actions carry no
                // prediction/evidence rows, and without them the measurements
                // scheduled next resolve into nothing.
                ensure_dispatch_envelope(
                    &mut transaction,
                    workspace_id,
                    action.id,
                    &action.payload,
                )
                .await?;
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
                UPDATE autopilot_actions
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
                INSERT INTO autopilot_action_attempts (
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
