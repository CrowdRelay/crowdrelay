//! First-party fan intelligence, segmentation, analytics and communication intent.
//!
//! This plane is intentionally read-heavy. It never sends provider mail in the
//! request path. Scheduling inserts one durable outbox event whose `available_at`
//! is the requested send time; downstream adapters resolve recipients only after
//! the event becomes due.

use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use crowdrelay_domain::venue_evidence::{
    EvidenceFact, EvidenceLocale, VenueAssessment, VenueEvidence, assess, unchecked_sentence,
};
use crowdrelay_domain::venue_terms::{
    TermsContribution, VenueTermsEvidence, aggregate_venue_terms,
};
use crowdrelay_infra::night::PostgresNightRepository;
use crowdrelay_infra::tenant_settings::TenantSettingsRepository;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{FromRow, Postgres, Transaction};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";
const MAX_LIST_LIMIT: i64 = 200;
const MAX_DELIVERY_PLAN_LIMIT: i64 = 500;

include!("audience/models.rs");

include!("audience/engagement_handlers.rs");
include!("audience/campaign_handlers.rs");
include!("audience/delivery_handlers.rs");
pub async fn funnel(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let now = OffsetDateTime::now_utc();
    let result = sqlx::query_as::<_, FunnelRow>(
        r#"
        WITH first_touch AS (
            SELECT DISTINCT ON (acquisition.fan_id)
                   acquisition.fan_id,
                   COALESCE(campaign.name, acquisition.source) AS source
            FROM fan_acquisition_events acquisition
            LEFT JOIN campaigns campaign
              ON campaign.workspace_id = acquisition.workspace_id
             AND campaign.id = acquisition.campaign_id
            WHERE acquisition.workspace_id = $1
            ORDER BY acquisition.fan_id, acquisition.occurred_at, acquisition.id
        ), fan_rollup AS (
            SELECT
                first_touch.source,
                fan.id AS fan_id,
                fan.status,
                fan.normalized_email,
                -- The real activation definition from crowdrelay_domain::fan_activation:
                -- consented AND at least one meaningful action inside 30 days.
                -- Account status 'active' is not activation — it is a statement
                -- about the account, not the person.
                EXISTS (
                    SELECT 1 FROM fan_consents AS consent
                    WHERE consent.workspace_id = fan.workspace_id
                      AND consent.fan_id = fan.id
                      AND consent.purpose = 'marketing'
                      AND consent.granted
                ) AS consented,
                fan_last_meaningful_action(fan.workspace_id, fan.id, fan.normalized_email)
                    AS last_action_at,
                EXISTS (
                    SELECT 1 FROM ticket_orders orders
                    WHERE orders.workspace_id = fan.workspace_id
                      AND orders.buyer_email = fan.normalized_email
                      AND orders.status IN ('paid', 'partially_refunded', 'refunded')
                ) AS bought,
                EXISTS (
                    SELECT 1 FROM admission_passes pass
                    WHERE pass.workspace_id = fan.workspace_id
                      AND pass.fan_id = fan.id
                      AND pass.status = 'redeemed'
                ) AS attended
            FROM first_touch
            JOIN fans fan
              ON fan.workspace_id = $1
             AND fan.id = first_touch.fan_id
        )
        SELECT source,
               count(*)::bigint AS acquired_fans,
               count(*) FILTER (
                   WHERE consented
                     AND last_action_at IS NOT NULL
                     AND last_action_at BETWEEN $2 - INTERVAL '30 days' AND $2
               )::bigint AS active_fans,
               count(*) FILTER (WHERE bought)::bigint AS ticket_buyers,
               count(*) FILTER (WHERE attended)::bigint AS attendees
        FROM fan_rollup
        GROUP BY source
        ORDER BY acquired_fans DESC, source
        LIMIT 100
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(now)
    .fetch_all(&state.database)
    .await;
    private_json(result, &headers)
}

pub async fn revenue(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let result = sqlx::query_as::<_, RevenueRow>(
        r#"
        SELECT orders.currency::text AS currency,
               count(*)::bigint AS paid_orders,
               sum(orders.amount_gross_minor)::bigint AS gross_paid_minor,
               sum(orders.amount_refunded_minor)::bigint AS refunded_minor,
               sum(orders.amount_gross_minor - orders.amount_refunded_minor)::bigint
                   AS after_refunds_minor
        FROM ticket_orders orders
        WHERE orders.workspace_id = $1
          AND orders.status IN ('paid', 'partially_refunded', 'refunded')
        GROUP BY orders.currency
        ORDER BY orders.currency
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .fetch_all(&state.database)
    .await;
    private_json(result, &headers)
}

/// Referral conversion readout: fan referrals plus the rolling Latarnik funnel.
///
/// The all-time fields remain backward compatible. The `latarnik_*_30d` fields
/// are the North Star read: what CrowdRelay offered, what the Latarnik tapped,
/// how many *people* actually followed the referral, and which of those people
/// became/behaved like fans. A tap is not a click, and a click is not a fan.
///
/// Mission attribution is intentionally bounded to a tapped mission's own
/// window. A generic referral by the same fan outside that window is still a
/// valid referral, but it is not allowed to make FAN SCOUT look effective.
pub async fn referral_conversion(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let now = OffsetDateTime::now_utc();
    let result = sqlx::query_as::<_, ReferralConversionRow>(
        r#"
        WITH all_referrals AS (
            SELECT
                count(*)::bigint AS referrals_sent,
                count(*) FILTER (WHERE ra.status = 'qualified')::bigint AS qualified,
                count(*) FILTER (
                    WHERE ra.status = 'qualified'
                      AND fan_last_meaningful_action(
                          referred.workspace_id, referred.id, referred.normalized_email
                      ) BETWEEN $2 - INTERVAL '30 days' AND $2
                      AND EXISTS (
                          SELECT 1 FROM fan_consents AS consent
                          WHERE consent.workspace_id = referred.workspace_id
                            AND consent.fan_id = referred.id
                            AND consent.purpose = 'marketing'
                            AND consent.granted
                      )
                )::bigint AS activated,
                count(*) FILTER (WHERE ra.status = 'reversed')::bigint AS reversed
            FROM referral_attributions ra
            JOIN fans AS referred
              ON referred.workspace_id = ra.workspace_id
             AND referred.id = ra.referred_fan_id
            WHERE ra.workspace_id = $1
        ),
        mission_windows AS (
            SELECT DISTINCT
                mission.id AS mission_id,
                mission.tapped_at,
                mission.expires_at,
                fan.id AS referrer_fan_id
            FROM latarnik_missions AS mission
            JOIN latarnik_roles AS role
              ON role.workspace_id = mission.workspace_id
             AND role.id = mission.role_id
            JOIN person_identities AS identity
              ON identity.workspace_id = role.workspace_id
             AND identity.person_id = role.person_id
             AND identity.kind = 'email'
             AND identity.platform IS NULL
            JOIN fans AS fan
              ON fan.workspace_id = identity.workspace_id
             AND fan.normalized_email = identity.value
            WHERE mission.workspace_id = $1
              AND mission.offered_at >= $2 - INTERVAL '30 days'
              AND mission.offered_at <= $2
        ),
        mission_counts AS (
            SELECT
                count(DISTINCT mission_id)::bigint AS offered,
                count(DISTINCT mission_id) FILTER (WHERE tapped_at IS NOT NULL)::bigint AS tapped
            FROM mission_windows
        ),
        mission_clickers AS (
            SELECT count(DISTINCT provenance.anonymous_visitor_id)::bigint AS human_clickers
            FROM fan_provenance_events AS provenance
            WHERE provenance.workspace_id = $1
              AND provenance.event_kind = 'interaction'
              AND provenance.channel = 'referral'
              AND provenance.attribution_method = 'referral_click'
              AND provenance.anonymous_visitor_id IS NOT NULL
              AND EXISTS (
                  SELECT 1
                  FROM mission_windows AS mission
                  WHERE mission.tapped_at IS NOT NULL
                    AND provenance.source_target = 'fan:' || mission.referrer_fan_id::text
                    AND provenance.occurred_at >= mission.tapped_at
                    AND provenance.occurred_at <= mission.expires_at + INTERVAL '7 days'
              )
        ),
        mission_referrals AS (
            SELECT DISTINCT
                referral.referred_fan_id,
                referral.status
            FROM referral_attributions AS referral
            WHERE referral.workspace_id = $1
              AND EXISTS (
                  SELECT 1
                  FROM mission_windows AS mission
                  WHERE mission.tapped_at IS NOT NULL
                    AND mission.referrer_fan_id = referral.referrer_fan_id
                    AND referral.accepted_at >= mission.tapped_at
                    AND referral.accepted_at <= mission.expires_at + INTERVAL '7 days'
              )
        ),
        mission_referral_counts AS (
            SELECT
                count(DISTINCT referral.referred_fan_id)::bigint AS joined,
                count(DISTINCT referral.referred_fan_id)
                    FILTER (WHERE referral.status = 'qualified')::bigint AS qualified,
                count(DISTINCT referral.referred_fan_id) FILTER (
                    WHERE EXISTS (
                        SELECT 1
                        FROM fans AS referred
                        WHERE referred.workspace_id = $1
                          AND referred.id = referral.referred_fan_id
                          AND fan_last_meaningful_action(
                              referred.workspace_id,
                              referred.id,
                              referred.normalized_email
                          ) BETWEEN $2 - INTERVAL '30 days' AND $2
                          AND EXISTS (
                              SELECT 1
                              FROM fan_consents AS consent
                              WHERE consent.workspace_id = referred.workspace_id
                                AND consent.fan_id = referred.id
                                AND consent.purpose = 'marketing'
                                AND consent.granted
                          )
                    )
                )::bigint AS activated
            FROM mission_referrals AS referral
        )
        SELECT
            all_referrals.referrals_sent,
            all_referrals.qualified,
            all_referrals.activated,
            all_referrals.reversed,
            mission_counts.offered AS latarnik_offered_30d,
            mission_counts.tapped AS latarnik_tapped_30d,
            mission_clickers.human_clickers AS latarnik_human_clickers_30d,
            mission_referral_counts.joined AS latarnik_joined_30d,
            mission_referral_counts.qualified AS latarnik_qualified_30d,
            mission_referral_counts.activated AS latarnik_activated_30d
        FROM all_referrals
        CROSS JOIN mission_counts
        CROSS JOIN mission_clickers
        CROSS JOIN mission_referral_counts
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(now)
    .fetch_one(&state.database)
    .await;
    private_json(result, &headers)
}

/// Per-city fan funnel: which cities have enough active fans to book a show.
///
/// The campaign plan's geographic loop: "two hundred in Wrocław, Kraków,
/// Poznań and Warszawa produce four shows." This endpoint tells the operator
/// which cities are close to that threshold, broken down by signups,
/// 30d-active, and consented fans. The `bookable` flag marks cities that
/// have crossed the minimum — 50 active fans, the plan's implied floor.
///
/// `new_30d` is the trend edge — fans who declared interest in the city
/// inside the last 30 days, so a city that is growing reads differently
/// from one that is merely large. `reachable` is the nearby-gig emitter's
/// own gate, mirrored exactly: location preference with nearby gigs
/// enabled, account `active`, marketing consent granted, and the fan's
/// city within their chosen radius of this one — the count of fans a
/// booked show there would actually page, not the count who follow it.
/// `venues`/`promoters`/`festivals` count the confirmed bookable targets
/// in that city — the "what's there" inventory beside the fans, so a
/// city with reach and no room reads as the gap it is.
/// `?order=organise` sorts by the domain's organise score — the ranked
/// answer to "which city do we organise in next" — instead of the
/// default 30d-active ordering. Anything else keeps the default.
#[derive(Debug, Deserialize)]
pub struct CityFunnelParams {
    order: Option<String>,
}

pub async fn city_funnel(
    State(state): State<crate::AppState>,
    Query(params): Query<CityFunnelParams>,
    headers: HeaderMap,
) -> Response {
    let now = OffsetDateTime::now_utc();
    let result = city_funnel_rows(
        &state.database,
        state.ticketing.workspace_id().into_uuid(),
        now,
        None,
    )
    .await
    .map(|mut rows| {
        if params.order.as_deref() == Some("organise") {
            rows.sort_by(|a, b| {
                b.organise_score_bp
                    .cmp(&a.organise_score_bp)
                    .then_with(|| b.active_30d.cmp(&a.active_30d))
                    .then_with(|| a.city_slug.cmp(&b.city_slug))
            });
        }
        rows
    });
    private_json(result, &headers)
}

/// The shared venue registry read (§4f-2): one row per room that any
/// tenant's published or completed event has marked, with the aggregates
/// the room record exists for — how many shows it has seen, how many
/// tenants have played it, what a night there typically draws through our
/// ticket sales, and how many fans keep coming back — plus the resolved
/// facts about the room (capacity, genres, website, address, status), each
/// carrying the provenance that won it and when it was observed.
/// Aggregates only: a mark is one tenant's private contribution, and
/// nothing on this row names a contributor — the facts are filtered to
/// `workspace_id IS NULL` so a contributor-private fact never leaks into
/// this cross-tenant read. `typical_draw` averages only shows that had a
/// ticket sale at all — an unticketed night is unmeasurable, not a zero.
/// `comparable_acts` (§12-5, 4V.6) counts distinct acts on this room's bills
/// — any tenant's — whose genre set intersects the requesting workspace's
/// own listing genres, resolved through the genre-alias map on both sides.
/// A name-only peer with no genre claims is honestly not counted: the count
/// is a floor, not a guess.
pub async fn city_venues(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    match city_venue_rows(&state, None).await {
        Ok(rows) => private_json(Ok::<_, sqlx::Error>(rows), &headers),
        Err(error) => {
            // Degrading the genre read to an empty set made every
            // comparability test fail and reported `comparable_acts: 0` — a
            // measured claim that no act of this band's genre ever played the
            // room. Any failed evidence read fails the request instead.
            tracing::warn!(%error, "city venues read failed");
            Problem::service_unavailable(request_id(&headers))
                .private()
                .into_response()
        }
    }
}

/// The standing verification brief — one paste-able prompt covering all
/// three registries the research loop feeds: rooms, bands, booking agents.
/// The worker's sheet answers on its own header and the Drive sync files
/// each back through its own reader — `status` facts retire dead rooms and
/// bands, the staged-status verdict flips `booking_agents.active`, and
/// discovered rows mint through their normal intakes.
///
/// `brief` is `null` only when all three registries are empty. The venue
/// and act lists are capped at 500 — a prompt is a paste target, not a
/// dump, and the registries are nowhere near that bound.
pub async fn registry_verification_brief(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let venues = sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT venue.display_name, city.name
        FROM place_venues AS venue
        JOIN cities AS city ON city.id = venue.city_id
        ORDER BY city.name, venue.display_name
        LIMIT 500
        "#,
    )
    .fetch_all(&state.database)
    .await;
    let Ok(venues) = venues else {
        return Problem::service_unavailable(request_id(&headers))
            .private()
            .into_response();
    };
    // Acts and agents ride the same brief — a band's home town resolves to
    // its catalogue city when one exists, so the worker can aim its checks.
    let acts = sqlx::query_as::<_, (String, Option<String>)>(
        r#"
        SELECT act.display_name, city.name
        FROM place_peer_acts AS act
        LEFT JOIN cities AS city ON city.id = act.home_city_id
        ORDER BY act.display_name
        LIMIT 500
        "#,
    )
    .fetch_all(&state.database)
    .await;
    let Ok(acts) = acts else {
        return Problem::service_unavailable(request_id(&headers))
            .private()
            .into_response();
    };
    // Only live agents are asked to verify — a retired one is already
    // resolved, and `active` is the flag the sync's verdict pass drives.
    let agents = sqlx::query_as::<_, (String, Option<String>, String)>(
        r#"
        SELECT name, agency, contact_email
        FROM booking_agents
        WHERE workspace_id = $1
          AND active
        ORDER BY name
        "#,
    )
    .bind(workspace_id)
    .fetch_all(&state.database)
    .await;
    let Ok(agents) = agents else {
        return Problem::service_unavailable(request_id(&headers))
            .private()
            .into_response();
    };
    // The tenant's genre narrows the discovery half — same read city_venues
    // runs, degrading to "no genre named" rather than failing the brief.
    let my_genres = sqlx::query_scalar::<_, Vec<String>>(
        r#"
        SELECT COALESCE(array_agg(DISTINCT lower(btrim(g))), '{}')
        FROM band_listings AS bl, unnest(bl.genre_tags) AS g
        WHERE bl.workspace_id = $1
        "#,
    )
    .bind(workspace_id)
    .fetch_one(&state.database)
    .await
    .unwrap_or_default();
    let genre = (!my_genres.is_empty()).then(|| my_genres.join(", "));
    private_json(
        Ok::<serde_json::Value, sqlx::Error>(serde_json::json!({
            "brief": crowdrelay_domain::venue_seed::verification_brief(
                &venues,
                &acts,
                &agents,
                genre.as_deref(),
            ),
        })),
        &headers,
    )
}

include!("audience/venue_assessment.rs");
include!("audience/city_reads.rs");

/// Ad conversion measurement: fan transfer from paid ad platforms into CrowdRelay.
///
/// Returns per-platform counts of attributed signups, successfully forwarded
/// conversion events, and the attribution-to-delivery funnel. This is the
/// control-plane readout that tells the operator whether their Meta/Google/
/// Bandsintown ad spend is actually converting into fans.
pub async fn ad_conversion_overview(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let workspace_uuid = state.ticketing.workspace_id().into_uuid();
    let result = sqlx::query_as::<_, AdConversionOverviewRow>(
        r#"
        WITH attributed AS (
            SELECT
                CASE
                    WHEN meta_fbp IS NOT NULL OR meta_fbc IS NOT NULL THEN true
                    ELSE false
                END AS has_meta,
                CASE
                    WHEN google_gclid IS NOT NULL THEN true
                    ELSE false
                END AS has_google,
                CASE
                    WHEN bandsintown_ref IS NOT NULL THEN true
                    ELSE false
                END AS has_bandsintown,
                CASE
                    WHEN utm_source IS NOT NULL THEN true
                    ELSE false
                END AS has_utm
            FROM fan_ad_attribution
            WHERE workspace_id = $1
        ),
        deliveries AS (
            SELECT
                platform,
                event_name,
                count(*)::bigint AS delivered,
                count(*) FILTER (WHERE response_status >= 200 AND response_status < 300)::bigint
                    AS delivered_ok
            FROM ad_conversion_deliveries
            WHERE workspace_id = $1
            GROUP BY platform, event_name
        )
        SELECT
            (SELECT count(*)::bigint FROM fan_ad_attribution WHERE workspace_id = $1)
                AS attributed_fans,
            (SELECT count(*)::bigint FROM attributed WHERE has_meta)::bigint
                AS meta_attributed,
            (SELECT count(*)::bigint FROM attributed WHERE has_google)::bigint
                AS google_attributed,
            (SELECT count(*)::bigint FROM attributed WHERE has_bandsintown)::bigint
                AS bandsintown_attributed,
            (SELECT count(*)::bigint FROM attributed WHERE has_utm)::bigint
                AS utm_attributed,
            COALESCE((
                SELECT delivered FROM deliveries WHERE platform = 'meta' AND event_name = 'Lead'
            ), 0)::bigint AS meta_lead_delivered,
            COALESCE((
                SELECT delivered_ok FROM deliveries WHERE platform = 'meta' AND event_name = 'Lead'
            ), 0)::bigint AS meta_lead_delivered_ok,
            COALESCE((
                SELECT delivered FROM deliveries WHERE platform = 'meta' AND event_name = 'Purchase'
            ), 0)::bigint AS meta_purchase_delivered,
            COALESCE((
                SELECT delivered_ok FROM deliveries WHERE platform = 'meta' AND event_name = 'Purchase'
            ), 0)::bigint AS meta_purchase_delivered_ok,
            COALESCE((
                SELECT delivered FROM deliveries WHERE platform = 'google' AND event_name = 'Lead'
            ), 0)::bigint AS google_lead_delivered,
            COALESCE((
                SELECT delivered_ok FROM deliveries WHERE platform = 'google' AND event_name = 'Lead'
            ), 0)::bigint AS google_lead_delivered_ok,
            COALESCE((
                SELECT delivered FROM deliveries WHERE platform = 'google' AND event_name = 'Purchase'
            ), 0)::bigint AS google_purchase_delivered,
            COALESCE((
                SELECT delivered_ok FROM deliveries WHERE platform = 'google' AND event_name = 'Purchase'
            ), 0)::bigint AS google_purchase_delivered_ok,
            COALESCE((
                SELECT delivered FROM deliveries WHERE platform = 'bandsintown' AND event_name = 'Lead'
            ), 0)::bigint AS bandsintown_lead_delivered,
            COALESCE((
                SELECT delivered_ok FROM deliveries WHERE platform = 'bandsintown' AND event_name = 'Lead'
            ), 0)::bigint AS bandsintown_lead_delivered_ok
        "#,
    )
    .bind(workspace_uuid)
    .fetch_one(&state.database)
    .await;
    private_json(result, &headers)
}

/// Per-platform conversion breakdown with UTM detail.
///
/// Returns one row per (platform, utm_source, utm_medium, utm_campaign)
/// combination, showing how many fans were attributed and how many
/// conversion events were successfully delivered. This lets the operator
/// compare ad campaigns side by side.
///
/// The query cross-joins attribution against the set of enabled platforms
/// so every UTM combination appears once per platform, even if no delivery
/// has happened yet — that's the "gap" the operator needs to see.
pub async fn ad_conversion_breakdown(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let workspace_uuid = state.ticketing.workspace_id().into_uuid();
    let result = sqlx::query_as::<_, AdConversionBreakdownRow>(
        r#"
        WITH attr AS (
            SELECT
                fan_id,
                COALESCE(NULLIF(utm_source, ''), '(unattributed)') AS utm_source,
                COALESCE(NULLIF(utm_medium, ''), '(unattributed)') AS utm_medium,
                COALESCE(NULLIF(utm_campaign, ''), '(unattributed)') AS utm_campaign
            FROM fan_ad_attribution
            WHERE workspace_id = $1
        ),
        -- One row per (platform, event_name) combination that we track.
        -- Bandsintown only has Lead; Meta and Google have both.
        platform_events AS (
            SELECT platform, event_name
            FROM (VALUES
                ('meta', 'Lead'),
                ('meta', 'Purchase'),
                ('google', 'Lead'),
                ('google', 'Purchase'),
                ('bandsintown', 'Lead')
            ) AS t(platform, event_name)
        ),
        utm_groups AS (
            SELECT
                utm_source,
                utm_medium,
                utm_campaign,
                count(DISTINCT fan_id)::bigint AS attributed_fans
            FROM attr
            GROUP BY utm_source, utm_medium, utm_campaign
        ),
        deliv AS (
            SELECT
                platform,
                event_name,
                fan_id,
                count(*)::bigint AS delivered,
                count(*) FILTER (WHERE response_status >= 200 AND response_status < 300)::bigint
                    AS delivered_ok
            FROM ad_conversion_deliveries
            WHERE workspace_id = $1
            GROUP BY platform, event_name, fan_id
        ),
        deliv_by_utm AS (
            SELECT
                deliv.platform,
                deliv.event_name,
                attr.utm_source,
                attr.utm_medium,
                attr.utm_campaign,
                COALESCE(sum(deliv.delivered), 0)::bigint AS delivered,
                COALESCE(sum(deliv.delivered_ok), 0)::bigint AS delivered_ok
            FROM attr
            JOIN deliv ON deliv.fan_id = attr.fan_id
            GROUP BY deliv.platform, deliv.event_name, attr.utm_source, attr.utm_medium, attr.utm_campaign
        )
        SELECT
            pe.platform,
            pe.event_name,
            utm.utm_source,
            utm.utm_medium,
            utm.utm_campaign,
            utm.attributed_fans,
            COALESCE(deliv.delivered, 0)::bigint AS delivered,
            COALESCE(deliv.delivered_ok, 0)::bigint AS delivered_ok
        FROM utm_groups utm
        CROSS JOIN platform_events pe
        LEFT JOIN deliv_by_utm deliv
          ON deliv.platform = pe.platform
         AND deliv.event_name = pe.event_name
         AND deliv.utm_source = utm.utm_source
         AND deliv.utm_medium = utm.utm_medium
         AND deliv.utm_campaign = utm.utm_campaign
        ORDER BY utm.attributed_fans DESC, pe.platform, pe.event_name, deliv.delivered_ok DESC
        LIMIT 200
        "#,
    )
    .bind(workspace_uuid)
    .fetch_all(&state.database)
    .await;
    private_json(result, &headers)
}

include!("audience/query_support.rs");
