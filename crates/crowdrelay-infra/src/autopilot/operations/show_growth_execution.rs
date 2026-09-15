//! Execution boundary for attendance-growth levers.
//!
//! First-party levers become normal consent-filtered communication campaigns.
//! External levers become a single executor intent; the executor may prepare or
//! publish the authorised artefact, but verified Beacon selection still belongs
//! to CrowdRelay and no cold destination is invented here.

use super::*;
use crowdrelay_domain::show_growth::ShowGrowthLever;

/// Canonical event facts a growth campaign renders from: slug, title, city
/// slug, venue, ticket URL, the event's own listen URL, and the announced bill
/// as a JSON array of `{slug, name}` in play order — `NULL` when no bill was
/// ever set, which renders as an empty lineup rather than a fabricated one.
type GrowthEventFacts = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<serde_json::Value>,
);

#[allow(clippy::too_many_arguments)]
pub(in crate::autopilot) async fn execute_show_growth(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    event_id: EventId,
    lever: ShowGrowthLever,
    template_key: &str,
    send_at: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let event = sqlx::query_as::<_, GrowthEventFacts>(
        r#"
        SELECT event.slug, event.title, city.slug, event.venue, event.ticket_url,
            event.listen_url,
            (SELECT jsonb_agg(jsonb_build_object('slug', act.act_slug, 'name', act.act_name)
                     ORDER BY act.position, act.act_slug)
             FROM event_acts AS act
             WHERE act.workspace_id = event.workspace_id
               AND act.event_id = event.id) AS acts
        FROM events AS event
        -- `cities` is a shared catalogue, not a tenant table: it has no
        -- `workspace_id`, and `events_city_id_fkey` references `cities(id)`
        -- alone. This join carried an `ON city.workspace_id = event.workspace_id`
        -- predicate, which made the statement fail to parse at all. The
        -- tenant boundary is the `event.workspace_id = $1` filter below, which
        -- is what every other join to `cities` in the workspace relies on.
        LEFT JOIN cities AS city
          ON city.id = event.city_id
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

    if lever == ShowGrowthLever::CanonicalLinkSetup {
        return ensure_canonical_show_link(tx, workspace_id, event_id, &event).await;
    }

    // A post-show lever only exists because the night is over: whichever one
    // executes first registers the show as harvestable material in the same
    // transaction. The harvest itself still waits out the material window in
    // the supply policy before drafting anything.
    if lever.is_post_show() {
        ensure_show_completed_source(tx, workspace_id, event_id).await?;
    }

    if lever.is_first_party_campaign() {
        return execute_first_party_growth_campaign(
            tx,
            workspace_id,
            event_id,
            action_id,
            &event,
            lever,
            template_key,
            send_at,
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
        "crowdrelay.show_growth.requested",
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

/// Gives the show a tracked link, or reactivates the one it already had.
///
/// Every other lever hands somebody a URL, and a URL nobody tracks turns all of
/// that work into an unmeasurable guess — the difference between "we shared it
/// and followers went up" and "forty people clicked". This is the cheapest
/// action in the whole system and the one the rest of the measurement rests on.
///
/// It writes only to the workspace's own link table, so it is safe to run
/// unattended: worst case it recreates a link that already pointed where it
/// should.
async fn ensure_canonical_show_link(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    event_id: EventId,
    event: &GrowthEventFacts,
) -> Result<(), RepositoryError> {
    // No destination, no link — and that is a normal state, not a failure. A
    // show that has not gone on sale yet has no ticket URL, which is most of
    // them most of the time. Treating it as an error retries five times and
    // then marks the action dead, turning ordinary quiet into a queue full of
    // corpses. The release version of this already returned Ok here; shows
    // were the inconsistent one.
    let Some(destination) = event.4.clone().filter(|url| url.starts_with("http")) else {
        return Ok(());
    };

    // The slug has to satisfy the smart-link pattern, and the event slug
    // already does; the prefix keeps agent-created links identifiable so an
    // operator can tell at a glance which links they made and which it did.
    let slug = format!("show-{}", event.0);
    if slug.len() > 128 {
        return Ok(());
    }

    sqlx::query(
        r#"
        INSERT INTO smart_links (workspace_id, slug, destination_url, active)
        VALUES ($1, $2, $3, true)
        ON CONFLICT (workspace_id, slug) DO UPDATE SET
            destination_url = EXCLUDED.destination_url,
            active = true,
            version = smart_links.version + 1
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&slug)
    .bind(&destination)
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    // The event row is the one place a later lever looks for the canonical
    // route, so record which link belongs to this show rather than leaving the
    // association implied by a naming convention.
    sqlx::query(
        r#"
        UPDATE viryaos_show_growth_surfaces
        SET attribution_url = $3, last_checked_at = now()
        WHERE workspace_id = $1 AND event_id = $2 AND attribution_url IS NULL
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(event_id.into_uuid())
    .bind(format!("/l/{slug}"))
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    Ok(())
}

/// Registers a finished show as a `show_completed` content source — the input
/// the harvest chain (recap, feed, story artifacts) demands. Called from every
/// first-party moment that proves the night ended while the event still sits
/// at `published`: a post-show lever firing, or reconciliation escalating. The
/// `source_key` mirrors the `viryaos_events_project_content_sources` trigger
/// that registers the same source on a `completed` flip, so helper, trigger
/// and operator upserts all converge on one row per show.
pub(in crate::autopilot) async fn ensure_show_completed_source(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    event_id: EventId,
) -> Result<(), RepositoryError> {
    sqlx::query(
        r#"
        INSERT INTO viryaos_content_sources (
            workspace_id, source_kind, source_key, title,
            occurred_at, expires_at, metadata, active
        )
        SELECT
            event.workspace_id, 'show_completed',
            'show_completed:' || event.id::text, event.title,
            -- `starts_at` rather than `now()`: the night is what is being
            -- harvested, and the supply policy's material window counts from
            -- when it ended, not from when some lever first noticed.
            event.starts_at, event.starts_at + interval '45 days',
            jsonb_build_object('event_id', event.id, 'slug', event.slug,
                'venue', event.venue, 'starts_at', event.starts_at,
                'city_id', event.city_id),
            true
        FROM events AS event
        WHERE event.workspace_id = $1
          AND event.id = $2
          AND event.status IN ('published','completed')
          AND event.starts_at <= now()
        ON CONFLICT (workspace_id, source_kind, source_key) DO NOTHING
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(event_id.into_uuid())
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    Ok(())
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
    send_at: Option<OffsetDateTime>,
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
        ShowGrowthLever::PostShowRecap => json!({
            "statuses": ["active"],
            "attended_event_slugs": [event.0.clone()],
            // Email-claim scans already hold the room's one next-morning
            // contact: their welcome doubles as the recall. Sending them the
            // recap too would put two messages in the same morning.
            "excluded_scan_checkin_event_slugs": [event.0.clone()],
            "marketing_consent": true
        }),
        ShowGrowthLever::PostShowFollowAsk => json!({
            "statuses": ["active"],
            "attended_event_slugs": [event.0.clone()],
            // Fans who scanned with an email claim already received the
            // night-of welcome, which doubles as their recall. Excluding them
            // here keeps the room at one contact inside the window instead of
            // a welcome at the door and a recall the next morning.
            "excluded_scan_checkin_event_slugs": [event.0.clone()],
            "marketing_consent": true
        }),
        _ => return Err(RepositoryError::Conflict),
    };

    let suffix = lever.as_str().replace('_', "-");
    // Segment and campaign slugs are CHECK-bounded at 128 chars while an
    // event slug may reach 128 itself; overlong events would fail the insert
    // and burn the lever's one shot. Truncate the event part so the lever
    // suffix — the part that makes campaigns distinct — always survives.
    let event_part: String = event.0.chars().take(90).collect();
    let slug = format!("viryaos-{}-{}", event_part, suffix);
    // `name` carries the event title into a 160-char CHECK bound.
    let display_name: String = event.1.chars().take(140).collect();
    let name = format!("{} · {}", display_name, lever.as_str());
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
        ShowGrowthLever::PostShowRecap => json!({
            "event_id": event_id,
            "lever": lever.as_str(),
            "venue": event.3,
            // The bill as announced, in play order: "here's who you saw" is a
            // statement of record, not a pitch, so it carries the acts table
            // rather than a ticket link.
            "acts": event.6.clone().unwrap_or_else(|| json!([])),
            "managed_by": "viryaos_show_growth",
            "email_contract": {
                "goal": "give the room one honest memory of the night — who played, where it was — so the band stays attached to the evening the fan actually had",
                "rules": [
                    "use_existing_marketing_consent_only",
                    "one_email_per_fan_per_show",
                    "no_ask_no_offer_no_link_farm_in_this_message",
                    "name_only_acts_on_the_announced_bill",
                    "do_not_claim_attendance_the_records_do_not_support",
                    "include_unsubscribe_via_existing_mailer_contract",
                    "never fabricate crowd_or_reaction_numbers"
                ]
            }
        }),
        ShowGrowthLever::PostShowFollowAsk => json!({
            "event_id": event_id,
            "lever": lever.as_str(),
            "ticket_url": event.4,
            "venue": event.3,
            "managed_by": "viryaos_show_growth",
            // Same canonical links as the pre-show push. The difference is who
            // receives it: people who were in the room, which is the warmest
            // list the band will ever have and the only one where "come with us
            // to the next one" is a statement of fact rather than a pitch.
            "growth_ctas": {
                "bandsintown_follow_url": "env:VIRYA_BANDSINTOWN_FOLLOW_URL",
                "spotify_artist_url": event
                    .5
                    .clone()
                    .unwrap_or_else(|| "env:VIRYA_SPOTIFY_ARTIST_URL".to_owned()),
                "spotify_playlist_url": "env:VIRYA_SPOTIFY_PLAYLIST_URL"
            },
            "email_contract": {
                "goal": "thank the people who were actually there and convert that night into a Spotify follow and a Bandsintown track, so the next show in their city finds them without paid reach",
                "rules": [
                    "use_existing_marketing_consent_only",
                    "one_email_per_fan_per_show",
                    "must_read_as_a_thank_you_first_and_an_ask_second",
                    "do_not_ask_for_money_in_this_message",
                    "do_not_claim_attendance_the_records_do_not_support",
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
    .bind(&name)
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
    .bind(&name)
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
        .bind(send_at.unwrap_or(now))
        .fetch_one(&mut **tx)
        .await
        .map_err(map_sqlx)?;
        sqlx::query(
            "UPDATE communication_campaigns SET status='scheduled',scheduled_at=$3,dispatch_event_id=$4 WHERE workspace_id=$1 AND id=$2 AND status='draft'",
        )
        .bind(workspace_id.into_uuid())
        .bind(campaign.0)
        .bind(send_at.unwrap_or(now))
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

/// Event row the T+7 report renders from — a wider fact set than the growth
/// facts because the artifact has to stand alone for a reader who never opens
/// the console: when, where, who played, and who sat across the table.
type ReportEventFacts = (
    String,
    String,
    Option<String>,
    Option<String>,
    OffsetDateTime,
    String,
    Option<String>,
    Option<String>,
    Option<serde_json::Value>,
);

/// Issues the T+7 post-show report: the night's numbers honestly labelled by
/// evidence class, delivered to the band and the event's counterparty as an
/// artifact email that needs no account. This is the escalation that
/// `post_show_report` resolves to — the report itself, not a reminder to go
/// write one — so the checklist item is marked done in the same transaction.
///
/// The numbers are split into `observed` (first-party room evidence: QR
/// check-ins, redeemed admission passes), `inferred` (signals that suggest
/// reach or attendance without proving presence: paid orders, interest,
/// clicks), and `evidence_gaps` (what cannot be claimed at all). A recipient
/// reading the artifact can repeat every figure without trusting us.
pub(in crate::autopilot) async fn issue_post_show_report(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    event_id: EventId,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let event = sqlx::query_as::<_, ReportEventFacts>(
        r#"
        SELECT event.slug, event.title, city.name, event.venue,
            event.starts_at, event.timezone,
            event.counterparty_name, event.counterparty_email,
            (SELECT jsonb_agg(jsonb_build_object('slug', act.act_slug, 'name', act.act_name)
                     ORDER BY act.position, act.act_slug)
             FROM event_acts AS act
             WHERE act.workspace_id = event.workspace_id
               AND act.event_id = event.id) AS acts
        FROM events AS event
        LEFT JOIN cities AS city ON city.id = event.city_id
        WHERE event.workspace_id = $1
          AND event.id = $2
          AND event.status IN ('published','completed')
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(event_id.into_uuid())
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::Conflict)?;

    // One pass over the room evidence and the proxy signals. `new_fan_records`
    // counts fans whose record first existed around show time and who checked
    // in — the stranger pipeline made flesh — rather than every email-claim
    // scan, which can also re-activate a long-pending fan.
    let numbers = sqlx::query_as::<_, (i64, i64, i64, i64, i64, i64, i64, i64)>(
        r#"
        SELECT
            (SELECT count(*) FROM concert_checkins AS c
             WHERE c.workspace_id = $1 AND c.event_id = $2) AS checkins_total,
            (SELECT count(*) FROM concert_checkins AS c
             WHERE c.workspace_id = $1 AND c.event_id = $2
               AND c.identity_source = 'session') AS checkins_session,
            (SELECT count(*) FROM concert_checkins AS c
             WHERE c.workspace_id = $1 AND c.event_id = $2
               AND c.identity_source = 'email_claim') AS checkins_email_claim,
            (SELECT count(*) FROM concert_checkins AS c
             JOIN fans AS fan
               ON fan.workspace_id = c.workspace_id AND fan.id = c.fan_id
             WHERE c.workspace_id = $1 AND c.event_id = $2
               AND fan.created_at >= (
                   SELECT starts_at - interval '6 hours'
                   FROM events WHERE workspace_id = $1 AND id = $2
               )
               -- Upper bound keeps a fan record created days later — with a
               -- checkin row for unrelated reasons — out of "new at show".
               AND fan.created_at <= (
                   SELECT starts_at + interval '12 hours'
                   FROM events WHERE workspace_id = $1 AND id = $2
               )) AS new_fan_records,
            (SELECT count(*) FROM admission_passes AS p
             WHERE p.workspace_id = $1 AND p.event_id = $2
               AND p.status = 'redeemed') AS passes_redeemed,
            (SELECT count(*) FROM event_action_events AS a
             WHERE a.workspace_id = $1 AND a.event_id = $2
               AND a.action = 'ticket_click') AS ticket_clicks,
            (SELECT count(DISTINCT lower(o.buyer_email)) FROM ticket_orders AS o
             JOIN ticket_sales AS s
               ON s.workspace_id = o.workspace_id AND s.id = o.ticket_sale_id
             WHERE o.workspace_id = $1 AND s.event_id = $2
               -- A partially refunded order is still a buyer who paid and
               -- (usually) came; 'refunded' alone drops out, matching the
               -- money-collected convention used elsewhere.
               AND o.status IN ('paid', 'partially_refunded')) AS paid_buyers,
            (SELECT count(*) FROM event_interests AS i
             WHERE i.workspace_id = $1 AND i.event_id = $2) AS interested_fans
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(event_id.into_uuid())
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    // Which act moved clicks — the per-act attribution the bill records, kept
    // separate from the total so a promoted support act's pull stays visible.
    let act_clicks = sqlx::query_as::<_, (Option<String>, i64)>(
        r#"
        SELECT act_slug, count(*) AS clicks
        FROM event_action_events
        WHERE workspace_id = $1 AND event_id = $2 AND action = 'ticket_click'
        GROUP BY act_slug
        ORDER BY clicks DESC, act_slug
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(event_id.into_uuid())
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    // What the system itself did about the night, with receipts — a campaign
    // that was scheduled but delivered nobody reads differently from one that
    // landed, and the report must not blur the two.
    let campaigns = sqlx::query_as::<
        _,
        (
            String,
            String,
            String,
            Option<OffsetDateTime>,
            Option<i32>,
            Option<i32>,
            Option<OffsetDateTime>,
        ),
    >(
        r#"
        SELECT slug, template_key, status, scheduled_at,
               recipient_count, delivered_count, completed_at
        FROM communication_campaigns
        WHERE workspace_id = $1 AND content->>'event_id' = $2
        ORDER BY scheduled_at NULLS LAST, slug
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(event_id.into_uuid().to_string())
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    // display_name is nullable on workspace_members — a member without one
    // still gets the report, labelled by the only identity they have.
    let band = sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT normalized_email, COALESCE(display_name, normalized_email)
        FROM workspace_members
        WHERE workspace_id = $1 AND status = 'active'
        ORDER BY display_name
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    let (
        checkins_total,
        checkins_session,
        checkins_email_claim,
        new_fan_records,
        passes_redeemed,
        ticket_clicks,
        paid_buyers,
        interested_fans,
    ) = numbers;

    let mut evidence_gaps: Vec<&str> = Vec::new();
    if checkins_total == 0 && passes_redeemed == 0 {
        // No scan and no redeemed pass: the night's attendance rests on
        // proxies alone, and the report must say so rather than quote zero.
        evidence_gaps.push("room_attendance_unverified");
    }
    if event.7.is_none() {
        evidence_gaps.push("no_counterparty_on_record");
    }
    if band.is_empty() {
        evidence_gaps.push("no_active_band_recipient");
    }
    if campaigns.is_empty() {
        // "No campaigns" is only true for THIS event's tag — a show with
        // untagged or staff-composed sends reads the same, so the gap names
        // the record, not the intent.
        evidence_gaps.push("no_event_campaigns_on_record");
    }

    crate::autopilot::emit_external_action(
        tx,
        workspace_id,
        action_id,
        "crowdrelay.show.post_show_report_due",
        json!({
            "action_id": action_id,
            "event_id": event_id,
            "event": {
                "slug": event.0,
                "title": event.1,
                "city": event.2,
                "venue": event.3,
                "starts_at": event.4,
                "timezone": event.5,
                "acts": event.8.unwrap_or_else(|| json!([])),
            },
            "report": {
                "kind": "post_show_t7",
                "generated_at": now,
                "observed": {
                    "room_checkins_total": checkins_total,
                    "room_checkins_by_session": checkins_session,
                    "room_checkins_by_email_claim": checkins_email_claim,
                    "new_fan_records_at_show": new_fan_records,
                    "admission_passes_redeemed": passes_redeemed,
                },
                "inferred": {
                    "paid_ticket_buyers": paid_buyers,
                    "interested_fans": interested_fans,
                    "ticket_link_clicks": ticket_clicks,
                    "ticket_link_clicks_by_act": act_clicks
                        .iter()
                        .map(|(slug, clicks)| json!({
                            "act_slug": slug,
                            "clicks": clicks,
                        }))
                        .collect::<Vec<_>>(),
                },
                "campaigns": campaigns
                    .iter()
                    .map(|row| json!({
                        "slug": row.0,
                        "template_key": row.1,
                        "status": row.2,
                        "scheduled_at": row.3,
                        "recipients": row.4,
                        "delivered": row.5,
                        "completed_at": row.6,
                    }))
                    .collect::<Vec<_>>(),
                "evidence_gaps": evidence_gaps,
            },
            "recipients": {
                "band": band
                    .iter()
                    .map(|(email, name)| json!({"email": email, "name": name}))
                    .collect::<Vec<_>>(),
                "counterparty": match (&event.6, &event.7) {
                    (_, Some(email)) => json!({"name": event.6, "email": email}),
                    _ => serde_json::Value::Null,
                },
            },
            "honesty_contract": {
                "observed": "first-party room evidence only — QR check-ins and redeemed admission passes",
                "inferred": "suggests reach or attendance but does not prove presence in the room",
                "rules": [
                    "never_sum_numbers_across_evidence_classes",
                    "state_evidence_gaps_explicitly_do_not_zero_them",
                    "do_not_claim_attendance_or_reach_the_records_do_not_support",
                    "the_artifact_is_the_whole_report_no_account_required"
                ]
            },
        }),
    )
    .await?;

    // The report shipping IS the task completing — same durable write the
    // auto-verify path uses, so re-evaluation holds instead of re-sending.
    sqlx::query(
        r#"INSERT INTO show_checklist_items(workspace_id,event_id,item_key,section,sort_order,status,note,updated_at)
           VALUES($1,$2,'post_show_report','post_show',320,'done','T+7 report issued by ViryaOS to band and counterparty',$3)
           ON CONFLICT(workspace_id,event_id,item_key) DO UPDATE
           SET status='done',note=EXCLUDED.note,updated_at=EXCLUDED.updated_at
           WHERE show_checklist_items.status<>'done'"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(event_id.into_uuid())
    .bind(now)
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    // A report going out proves the night ended; if reconciliation never ran
    // (an odd path, but possible) the harvest source still registers here.
    ensure_show_completed_source(tx, workspace_id, event_id).await
}
