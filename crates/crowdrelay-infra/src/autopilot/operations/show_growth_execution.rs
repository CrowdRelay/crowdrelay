//! Execution boundary for attendance-growth levers.
//!
//! First-party levers become normal consent-filtered communication campaigns.
//! External levers become a single executor intent; the executor may prepare or
//! publish the authorised artefact, but verified Beacon selection still belongs
//! to CrowdRelay and no cold destination is invented here.

use super::*;
use crowdrelay_domain::show_growth::ShowGrowthLever;

pub(in crate::autopilot) async fn execute_show_growth(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    event_id: EventId,
    lever: ShowGrowthLever,
    template_key: &str,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let event = sqlx::query_as::<
        _,
        (
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        ),
    >(
        r#"
        SELECT event.slug, event.title, city.slug, event.venue, event.ticket_url
        FROM events AS event
        LEFT JOIN cities AS city
          ON city.workspace_id = event.workspace_id
         AND city.id = event.city_id
        WHERE event.workspace_id = $1
          AND event.id = $2
          AND event.status IN ('published','completed')
        FOR UPDATE OF event
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(event_id.into_uuid())
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::Conflict)?;

    if lever.is_first_party_campaign() {
        return execute_first_party_growth_campaign(
            tx,
            workspace_id,
            event_id,
            action_id,
            &event,
            lever,
            template_key,
            now,
        )
        .await;
    }

    let constraints = match lever {
        ShowGrowthLever::FreeListingSweep => json!({
            "objective": "make the canonical show discoverable on every relevant free/owned surface before asking for more reach",
            "surface_classes": [
                "virya_canonical_show_page",
                "bandsintown_event_with_ticket_link",
                "songkick_tourbox_event_with_ticket_link",
                "spotify_live_event_visibility_via_supported_ticket_partner_or_bandsintown",
                "google_event_visibility_via_bandsintown_distribution",
                "youtube_official_artist_concert_visibility_via_bandsintown_distribution",
                "apple_music_maps_shazam_visibility_via_bandsintown_distribution",
                "venue_calendar_or_newsletter",
                "local_city_or_culture_calendar",
                "relevant_free_scene_calendar"
            ],
            "rules": [
                "free_or_owned_channels_only",
                "use_canonical_event_facts_and_ticket_url",
                "deduplicate_by_destination_and_event",
                "verify_existing_listing_before_creating_or_updating",
                "treat_bandsintown_as_one_canonical_distribution_source_not_five_duplicate_manual_posts",
                "use_songkick_tourbox_as_an_independent_free_discovery_graph_when_artist_access_is_available",
                "verify_downstream_distribution_health_and_return_drift_as_manual_steps",
                "respect_destination_terms_and_moderation",
                "never_bypass_captcha_login_or_email_verification",
                "return_public_urls_for_every_success",
                "return_manual_steps_for_surfaces_requiring_human_action",
                "never_purchase_placement_without_separate_approval"
            ],
            "receipt_contract": {
                "metadata": ["checked_surfaces", "published_urls", "distribution_health", "manual_steps", "skipped_with_reason"],
                "manual_steps_must_include": ["destination", "url", "what_to_do", "why_it_matters"]
            }
        }),
        ShowGrowthLever::AudienceCaptureSetup => json!({
            "objective": "capture free show intent on provider-native discovery surfaces while keeping VIRYA Signal the primary first-party fan relationship",
            "surface_classes": [
                "bandsintown_smart_link_with_canonical_event_and_ticket_url",
                "bandsintown_follow_or_signup_surface",
                "bandsintown_event_widget_follow_or_signup",
                "bandsintown_qr_for_social_or_physical_materials",
                "bandsintown_presale_signup_or_event_reminder_when_applicable",
                "virya_signal_primary_signup_on_owned_surfaces"
            ],
            "rules": [
                "free_or_owned_channels_only",
                "keep_virya_signal_primary_on_owned_surfaces",
                "do_not_import_signal_contacts_into_third_party_tools_without_explicit_policy_and_consent",
                "use_canonical_event_facts_and_ticket_url",
                "if_presale_is_not_configured_do_not_invent_dates_codes_or_access",
                "if_ticketing_is_not_on_sale_use_supported_reminder_or_signup_only_when_truthfully_applicable",
                "never_bypass_login_2fa_captcha_or_email_verification",
                "return_manual_steps_for_human_only_provider_configuration",
                "never_purchase_promotion_or_upgrade_without_separate_approval"
            ],
            "receipt_contract": {
                "metadata": ["checked_surfaces", "configured_urls", "capture_surfaces", "manual_steps", "skipped_with_reason"],
                "manual_steps_must_include": ["destination", "url", "what_to_do", "why_it_matters"]
            }
        }),
        ShowGrowthLever::PartnerCrossPromo => json!({
            "objective": "borrow relevant local audiences through venue, bill and scene partners instead of making VIRYA carry discovery alone",
            "preferred_actions": [
                "venue_calendar_or_newsletter",
                "venue_or_promoter_co_post",
                "facebook_page_event_cohost_or_calendar_relay",
                "instagram_collab_or_co_post_when_partner_accepts",
                "support_or_bill_cross_post",
                "scene_community_listing",
                "ticket_giveaway_with_verified_partner",
                "shared_short_form_live_clip"
            ],
            "rules": [
                "discover_or_use_verified_beacons_only",
                "prefer_venue_scene_partner_promoter_community",
                "ingest_new_public_candidates_as_unverified_beacons_before_contact",
                "actual_outreach_must_pass_beacon_policy_and_suppression",
                "one_concrete_ask_per_message",
                "share_one_canonical_ticket_url",
                "no_reciprocal_commitment_outside_configured_authority"
            ]
        }),
        ShowGrowthLever::FreeFanChannelPush => json!({
            "objective": "use free provider-native follower channels to put this show in front of already-relevant listeners without paid reach",
            "surface_classes": [
                "bandsintown_location_targeted_post",
                "bandsintown_event_rsvp_targeted_post",
                "bandsintown_email_builder_local_followers_within_verified_free_quota",
                "spotify_artist_pick_for_event"
            ],
            "rules": [
                "free_quota_or_free_surface_only",
                "verify_current_provider_quota_before_sending_email",
                "never_use_bandsintown_boost_or_promoted_campaign_without_separate_paid_approval",
                "do_not_import_signal_contacts_into_bandsintown_for_this_action",
                "target_only_existing_provider_followers_or_event_rsvps_by_location_or_event",
                "one_canonical_ticket_url",
                "use_only_verified_event_facts_and_approved_copy",
                "spotify_artist_pick_is_a_human_provider_configuration_step_unless_an_official_supported_api_exists",
                "never_bypass_login_2fa_captcha_or_email_verification",
                "return_manual_steps_when_provider_ui_action_is_required"
            ],
            "receipt_contract": {
                "metadata": ["checked_surfaces", "scheduled_or_sent", "provider_reach", "manual_steps", "skipped_with_reason"],
                "manual_steps_must_include": ["destination", "url", "what_to_do", "why_it_matters"]
            }
        }),
        ShowGrowthLever::SocialProofRelay => json!({
            "objective": "relay truthful proof and a strong local reason to attend across owned/partner channels",
            "preferred_proof": [
                "verified_review_or_interview",
                "verified_patronage_or_media_partner",
                "strong_current_live_video",
                "real_live_photo_or_crowd_moment",
                "verified_festival_final_or_award_fact"
            ],
            "rules": [
                "approved_first_party_facts_only",
                "never_invent_quotes_reviews_streams_or_sold_out_claims",
                "adapt_format_per_channel_without_changing_claims",
                "prefer_live_video_real_crowd_or_verified_press_proof",
                "if_no_verified_proof_exists_use_local_story_context_not_fake_social_proof",
                "one_canonical_ticket_cta"
            ]
        }),
        _ => return Err(RepositoryError::Conflict),
    };

    crate::autopilot::emit_external_action(
        tx,
        workspace_id,
        action_id,
        "viryaos.show_growth.requested",
        json!({
            "action_id": action_id,
            "event_id": event_id,
            "event_slug": event.0,
            "event_title": event.1,
            "city_slug": event.2,
            "venue": event.3,
            "ticket_url": event.4,
            "lever": lever,
            "template_key": template_key,
            "constraints": constraints,
        }),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn execute_first_party_growth_campaign(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    event_id: EventId,
    action_id: crowdrelay_domain::AutopilotActionId,
    event: &(
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    ),
    lever: ShowGrowthLever,
    template_key: &str,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let enabled = sqlx::query_scalar::<_, bool>(
        "SELECT COALESCE((SELECT enabled FROM ecosystem_feature_flags WHERE workspace_id=$1 AND key='communication_campaigns_enabled'),false)",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    if !enabled {
        return Err(RepositoryError::Conflict);
    }

    let filter = match lever {
        ShowGrowthLever::FanAmbassadors => json!({
            "statuses": ["active"],
            "city_slugs": event.2.clone().into_iter().collect::<Vec<_>>(),
            "min_qualified_referrals": 1,
            "marketing_consent": true
        }),
        ShowGrowthLever::MerchBuyerOffer => json!({
            "statuses": ["active"],
            "purchased_event_slugs": [event.0.clone()],
            "marketing_consent": true,
            "offer_contract": {
                "audience": "ticket_buyers",
                "objective": "convert existing show intent into merch revenue before the event",
                "fulfilment": "use_current_commerce_options_only",
                "never_promise_event_pickup_without_checkout_support": true
            }
        }),
        ShowGrowthLever::HighIntentLastMile => json!({
            "statuses": ["active"],
            "interested_event_slugs": [event.0.clone()],
            "excluded_purchased_event_slugs": [event.0.clone()],
            "marketing_consent": true
        }),
        ShowGrowthLever::PostShowMerchFollowUp => json!({
            "statuses": ["active"],
            "attended_event_slugs": [event.0.clone()],
            "marketing_consent": true
        }),
        _ => return Err(RepositoryError::Conflict),
    };

    let suffix = lever.as_str().replace('_', "-");
    let slug = format!("viryaos-{}-{}", event.0, suffix);
    let segment_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO audience_segments(workspace_id,slug,name,description,filter,active)
        VALUES($1,$2,$3,'ViryaOS attendance-growth segment',$4,true)
        ON CONFLICT(workspace_id,slug) DO UPDATE SET filter=EXCLUDED.filter,active=true
        RETURNING id
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&slug)
    .bind(format!("{} · {}", event.1, lever.as_str()))
    .bind(filter)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    let campaign = sqlx::query_as::<_, (Uuid, String)>(
        r#"
        INSERT INTO communication_campaigns(
            workspace_id,segment_id,slug,name,channel,template_key,content
        ) VALUES(
            $1,$2,$3,$4,'email',$5,
            jsonb_build_object(
                'event_id',$6::uuid,
                'lever',$7::text,
                'ticket_url',$8::text,
                'venue',$9::text,
                'managed_by','viryaos_show_growth'
            )
        )
        ON CONFLICT(workspace_id,slug) DO UPDATE SET template_key=communication_campaigns.template_key
        RETURNING id,status
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(segment_id)
    .bind(&slug)
    .bind(format!("{} · {}", event.1, lever.as_str()))
    .bind(template_key)
    .bind(event_id.into_uuid())
    .bind(lever.as_str())
    .bind(&event.4)
    .bind(&event.3)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    if campaign.1 == "draft" {
        let outbox_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO outbox_events(workspace_id,event_type,event_version,payload,available_at)
            VALUES(
                $1,'communication.campaign_due',1,
                jsonb_build_object(
                    'campaign_id',$2::uuid,'campaign_slug',$3::text,'channel','email',
                    'segment_id',$4::uuid,'template_key',$5::text
                ),$6
            ) RETURNING id
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(campaign.0)
        .bind(&slug)
        .bind(segment_id)
        .bind(template_key)
        .bind(now)
        .fetch_one(&mut **tx)
        .await
        .map_err(map_sqlx)?;
        sqlx::query(
            "UPDATE communication_campaigns SET status='scheduled',scheduled_at=$3,dispatch_event_id=$4 WHERE workspace_id=$1 AND id=$2 AND status='draft'",
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

    let _ = action_id; // action id is already the durable one-shot record.
    Ok(())
}
