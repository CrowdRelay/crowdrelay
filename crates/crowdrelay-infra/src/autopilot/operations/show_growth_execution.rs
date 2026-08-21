//! Execution boundary for attendance-growth levers.
//!
//! First-party levers become normal consent-filtered communication campaigns.
//! External levers become a single executor intent; the executor may prepare or
//! publish the authorised artefact, but verified Beacon selection still belongs
//! to CrowdRelay and no cold destination is invented here.

use super::*;
use crowdrelay_domain::show_growth::ShowGrowthLever;

/// Canonical event facts a growth campaign renders from: slug, title, city
/// slug, venue, ticket URL and the event's own listen URL.
type GrowthEventFacts = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

pub(in crate::autopilot) async fn execute_show_growth(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    event_id: EventId,
    lever: ShowGrowthLever,
    template_key: &str,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let event = sqlx::query_as::<_, GrowthEventFacts>(
        r#"
        SELECT event.slug, event.title, city.slug, event.venue, event.ticket_url, event.listen_url
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
                "amazon_music_event_visibility_via_bandsintown_distribution",
                "songkick_partner_distribution_health_deezer_bandcamp_soundcloud",
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
                "verify_bandsintown_artist_links_for_spotify_youtube_official_artist_channel_and_apple_music_before_assuming_distribution",
                "verify_youtube_sell_tickets_setting_when_official_artist_channel_is_available",
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
                "virya_signal_primary_signup_on_owned_surfaces",
                "virya_signal_signup_qr_for_merch_table_current_shows_and_permitted_partner_surfaces",
                "canonical_show_qr_with_campaign_attribution"
            ],
            "rules": [
                "free_or_owned_channels_only",
                "keep_virya_signal_primary_on_owned_surfaces",
                "do_not_import_signal_contacts_into_third_party_tools_without_explicit_policy_and_consent",
                "use_canonical_event_facts_and_ticket_url",
                "if_presale_is_not_configured_do_not_invent_dates_codes_or_access",
                "if_ticketing_is_not_on_sale_use_supported_reminder_or_signup_only_when_truthfully_applicable",
                "physical_or_partner_qr_placement_requires_venue_or_owner_permission",
                "owned_qr_must_preserve_normal_signal_consent_and_use_campaign_attribution_when_available",
                "do_not_purchase_printing_or_placement_without_separate_authority",
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
                "ask_verified_scene_beacon_for_one_warm_intro_to_one_relevant_local_scene_contact",
                "moderated_local_metal_group_or_forum_listing_when_rules_allow",
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
                "warm_intro_requires_beacon_consent_and_no_private_contact_data_is_forwarded_without_permission",
                "new_intro_target_enters_as_unverified_beacon_until_public_identity_or_direct_consent_is_verified",
                "community_posts_are_manual_or_moderator_approved_and_never_automated_cold_spam",
                "never_bypass_group_rules_posting_limits_or_moderation",
                "no_reciprocal_commitment_outside_configured_authority"
            ],
            "receipt_contract": {
                "activation_fields": ["activation_kind", "destination_key", "status", "reply_received"],
                "reply_received_semantics": "true_only_after_an_explicit_inbound_human_reply"
            }
        }),
        ShowGrowthLever::GrassrootsSceneRelay => json!({
            "objective": "activate a small trusted local scene graph around the show so reach comes through real relationships rather than anonymous cold promotion",
            "surface_classes": [
                "verified_local_metal_media_or_podcast",
                "independent_radio_or_student_music_programme",
                "record_store",
                "rehearsal_studio",
                "music_shop_or_luthier",
                "tattoo_alt_fashion_or_scene_business",
                "local_live_photographer_or_video_creator",
                "venue_promoter_or_bill_partner",
                "moderated_local_metal_community",
                "student_or_city_culture_channel",
                "fan_ambassador_with_existing_local_trust"
            ],
            "preferred_actions": [
                "one_useful_cross_post_or_listing",
                "one_consent_based_warm_intro",
                "one_physical_signal_or_show_qr_placement_with_owner_permission",
                "one_ticket_giveaway_with_verified_partner_and_explicit_terms",
                "one_permissioned_live_clip_or_photo_relay",
                "one_local_story_angle_tied_to_the_show_or_scene"
            ],
            "rules": [
                "verified_existing_beacons_may_be_contacted_only_through_normal_beacon_policy",
                "new_public_candidates_are_ingested_as_unverified_before_any_outreach",
                "prefer_depth_of_relationship_over_number_of_contacts",
                "one_concrete_ask_per_person_or_community",
                "warm_intro_requires_explicit_consent_from_the_introducing_beacon",
                "never_forward_private_contact_data_without_permission",
                "community_posting_is_manual_or_moderator_approved_only",
                "physical_qr_or_flyer_placement_requires_owner_or_venue_permission",
                "use_campaign_attributed_canonical_urls_or_signal_qr_where_supported",
                "fan_generated_media_requires_explicit_repost_permission_and_credit",
                "do_not_offer_money_freebies_or_reciprocity_not_already_authorised",
                "no_scraping_no_mass_dm_no_automated_cold_group_posting"
            ],
            "relationship_state_contract": {
                "edge_kinds": ["warm_intro", "cross_promo", "community_access", "bill_partner", "venue_partner", "creator_relay"],
                "statuses": ["candidate", "permission_requested", "introduced", "active", "declined", "suppressed"],
                "measure": ["attributable_reach", "clicks", "rsvps", "ticket_orders"]
            },
            "receipt_contract": {
                "metadata": ["activated_relationships", "warm_intros", "public_urls", "qr_placements", "manual_steps", "skipped_with_reason"],
                "activation_fields": ["activation_kind", "destination_key", "status", "reply_received"],
                "reply_received_semantics": "true_only_after_an_explicit_inbound_human_reply",
                "manual_steps_must_include": ["destination", "what_to_do", "why_it_matters", "consent_or_moderation_requirement"]
            }
        }),
        // FreeFanChannelPush is a first-party lever: execute_show_growth
        // returns into execute_first_party_growth_campaign before this
        // match, so it never reached an arm here. The provider-surface
        // policy that used to sit here was therefore never emitted to any
        // executor; the campaign content carries the real contract now.
        ShowGrowthLever::SocialProofRelay => json!({
            "objective": "relay truthful proof and a strong local reason to attend across owned/partner channels",
            "preferred_proof": [
                "verified_review_or_interview",
                "verified_patronage_or_media_partner",
                "strong_current_live_video",
                "real_live_photo_or_crowd_moment",
                "fan_generated_live_photo_or_clip_with_explicit_repost_permission",
                "verified_festival_final_or_award_fact"
            ],
            "rules": [
                "approved_first_party_facts_only",
                "never_invent_quotes_reviews_streams_or_sold_out_claims",
                "adapt_format_per_channel_without_changing_claims",
                "prefer_live_video_real_crowd_or_verified_press_proof",
                "fan_generated_media_requires_explicit_repost_permission_and_credit",
                "never_repurpose_private_or_closed_group_media_without_permission",
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
    event: &GrowthEventFacts,
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
        ShowGrowthLever::FreeFanChannelPush => {
            if let Some(city_slug) = event.2.clone() {
                json!({
                    "statuses": ["active"],
                    "city_slugs": [city_slug],
                    "marketing_consent": true
                })
            } else {
                json!({
                    "statuses": ["active"],
                    "interested_event_slugs": [event.0.clone()],
                    "marketing_consent": true
                })
            }
        }
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
    let content = match lever {
        ShowGrowthLever::FanAmbassadors => json!({
            "event_id": event_id,
            "lever": lever.as_str(),
            "ticket_url": event.4,
            "venue": event.3,
            "managed_by": "viryaos_show_growth",
            "relay_pack": {
                "objective": "help a small number of proven local fans reach real metalheads through personal trust rather than mass promotion",
                "preferred_actions": [
                    "send_a_personal_invite_to_one_to_three_relevant_friends",
                    "share_the_canonical_event_link_in_a_personal_story_or_feed",
                    "share_in_a_local_metal_community_only_when_the_fan_is_already_a_member_and_rules_allow_it",
                    "make_an_optional_short_using_official_virya_audio_if_the_fan_already_creates_public_content",
                    "share_the_campaign_attributed_signal_or_show_qr_at_an_in_person_scene_touchpoint_when_permission_exists",
                    "use_the_fans_existing_referral_identity_when_the_delivery_template_supports_it"
                ],
                "rules": [
                    "no_mass_dm_or_contact_scraping",
                    "no_automated_group_posting",
                    "no_fake_urgency_or_fake_social_proof",
                    "one_canonical_ticket_url",
                    "respect_community_rules_and_moderators",
                    "do_not_create_new_financial_incentives_without_separate_authority"
                ]
            }
        }),
        ShowGrowthLever::FreeFanChannelPush => json!({
            "event_id": event_id,
            "lever": lever.as_str(),
            "ticket_url": event.4,
            "venue": event.3,
            "managed_by": "viryaos_show_growth",
            // The event already carries its canonical listen URL, so send the
            // real value rather than an `env:` placeholder the executor may not
            // be able to resolve. An unresolvable CTA silently drops out of the
            // message, and delivering exactly these links is what this lever is
            // for. The other two stay deferred: CrowdRelay holds no
            // provider-native follow or playlist URL of its own.
            "growth_ctas": {
                "bandsintown_follow_url": "env:VIRYA_BANDSINTOWN_FOLLOW_URL",
                "spotify_artist_url": event
                    .5
                    .clone()
                    .unwrap_or_else(|| "env:VIRYA_SPOTIFY_ARTIST_URL".to_owned()),
                "spotify_playlist_url": "env:VIRYA_SPOTIFY_PLAYLIST_URL"
            },
            "email_contract": {
                "goal": "convert already-consented local fans into Spotify followers/listeners and Bandsintown followers without paid reach",
                "rules": [
                    "use_existing_marketing_consent_only",
                    "one_email_per_fan_per_show_growth_wave",
                    "do_not_claim_exclusive_access_unless_true",
                    "include_unsubscribe_via_existing_mailer_contract",
                    "prefer_one_primary_cta_and_one_secondary_cta",
                    "never fabricate follower_or_stream_numbers"
                ]
            }
        }),
        _ => json!({
            "event_id": event_id,
            "lever": lever.as_str(),
            "ticket_url": event.4,
            "venue": event.3,
            "managed_by": "viryaos_show_growth"
        }),
    };
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
        ) VALUES($1,$2,$3,$4,'email',$5,$6)
        ON CONFLICT(workspace_id,slug) DO UPDATE SET template_key=communication_campaigns.template_key
        RETURNING id,status
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(segment_id)
    .bind(&slug)
    .bind(format!("{} · {}", event.1, lever.as_str()))
    .bind(template_key)
    .bind(content)
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
