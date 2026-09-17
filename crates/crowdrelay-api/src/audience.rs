//! First-party fan intelligence, segmentation, analytics and communication intent.
//!
//! This plane is intentionally read-heavy. It never sends provider mail in the
//! request path. Scheduling inserts one durable outbox event whose `available_at`
//! is the requested send time; downstream adapters resolve recipients only after
//! the event becomes due.

use axum::{
    Json,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
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

/// Referral conversion readout: sent → qualified → activated.
///
/// The campaign plan asks for "referral conversion" and "activated referral
/// rate". This endpoint gives the funnel: how many people used a referral
/// code, how many of those qualified, and how many of the qualified
/// referrals are themselves 30d-active. The last number is the one that
/// matters — a referral who signed up but never did anything is not an
/// activated referral.
pub async fn referral_conversion(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let now = OffsetDateTime::now_utc();
    let result = sqlx::query_as::<_, ReferralConversionRow>(
        r#"
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
    let result = sqlx::query_as::<_, CityFunnelRow>(
        r#"
        WITH city_fans AS (
            SELECT
                city.id AS city_id,
                city.slug AS city_slug,
                city.name AS city_name,
                city.country_code,
                city.region,
                fan.normalized_email,
                interest.created_at AS interest_created_at,
                -- fan_consents is append-only: consent means the LATEST
                -- row for the purpose is granted, not that any row ever
                -- was — a check-in withdrawal must drop the fan out of
                -- every count on this row.
                EXISTS (
                    SELECT 1 FROM fan_consents AS consent
                    WHERE consent.workspace_id = fan.workspace_id
                      AND consent.fan_id = fan.id
                      AND consent.purpose = 'marketing'
                      AND consent.granted
                      AND consent.id = (
                          SELECT newest.id
                          FROM fan_consents AS newest
                          WHERE newest.workspace_id = consent.workspace_id
                            AND newest.fan_id = consent.fan_id
                            AND newest.purpose = consent.purpose
                          ORDER BY newest.recorded_at DESC, newest.id DESC
                          LIMIT 1
                      )
                ) AS consented,
                fan_last_meaningful_action(
                    fan.workspace_id, fan.id, fan.normalized_email
                ) AS last_meaningful_action_at
            FROM fan_city_interests AS interest
            JOIN cities AS city
              ON city.id = interest.city_id
            JOIN fans AS fan
              ON fan.workspace_id = interest.workspace_id
             AND fan.id = interest.fan_id
            WHERE interest.workspace_id = $1
              -- 'merged' rows are dedup tombstones whose identity moved
              -- to the surviving fan; everything else, pending included,
              -- is a real member of the fanbase a city count should name.
              AND fan.status <> 'merged'
        ),
        agg AS (
            SELECT
                city_id,
                city_slug,
                city_name,
                country_code,
                region,
                count(*)::bigint AS fans,
                count(*) FILTER (
                    WHERE interest_created_at BETWEEN $2 - INTERVAL '30 days' AND $2
                )::bigint AS new_30d,
                count(*) FILTER (
                    WHERE last_meaningful_action_at IS NOT NULL
                      AND last_meaningful_action_at BETWEEN $2 - INTERVAL '30 days' AND $2
                      AND consented
                )::bigint AS active_30d,
                count(*) FILTER (WHERE consented)::bigint AS consented
            FROM city_fans
            GROUP BY city_id, city_slug, city_name, country_code, region
        ),
        reachable AS (
            -- The nearby-gig gate applied city-by-city over the funnel's
            -- own candidate set: fans whose location preference is enabled,
            -- account active, marketing consent granted, and whose city sits
            -- inside their radius of this one. A fan following Kraków from
            -- Katowice still counts for Kraków — the emitter would page them.
            SELECT
                agg.city_id,
                count(*)::bigint AS reachable
            FROM agg
            JOIN cities AS city
              ON city.id = agg.city_id
             AND city.latitude IS NOT NULL
             AND city.longitude IS NOT NULL
            JOIN fan_location_preferences AS preferences
              ON preferences.workspace_id = $1
             AND preferences.nearby_gigs_enabled
            JOIN cities AS fan_city
              ON fan_city.id = preferences.city_id
             AND fan_city.latitude IS NOT NULL
             AND fan_city.longitude IS NOT NULL
            JOIN fans AS fan
              ON fan.workspace_id = preferences.workspace_id
             AND fan.id = preferences.fan_id
             AND fan.status = 'active'
            WHERE EXISTS (
                SELECT 1
                FROM fan_consents AS consent
                WHERE consent.workspace_id = fan.workspace_id
                  AND consent.fan_id = fan.id
                  AND consent.purpose = 'marketing'
                  AND consent.granted
                  AND consent.id = (
                      SELECT newest.id
                      FROM fan_consents AS newest
                      WHERE newest.workspace_id = consent.workspace_id
                        AND newest.fan_id = consent.fan_id
                        AND newest.purpose = consent.purpose
                      ORDER BY newest.recorded_at DESC, newest.id DESC
                      LIMIT 1
                  )
            )
              -- The emitter's own bound: one degree of latitude is
              -- 111.19 km wherever you stand, so a pair further apart
              -- than the radius in latitude alone can never be inside it.
              AND abs(fan_city.latitude - city.latitude)
                  <= (preferences.radius_km + 1)::double precision / 111.0
              -- Rounded, matching the emitter's distance_km comparison: a
              -- fan at 50.4 km with radius 50 is paged, not dropped.
              AND ROUND(6371 * 2 * ASIN(LEAST(1.0, SQRT(
                    POWER(SIN(RADIANS(fan_city.latitude - city.latitude) / 2), 2)
                    + COS(RADIANS(city.latitude)) * COS(RADIANS(fan_city.latitude))
                    * POWER(SIN(RADIANS(fan_city.longitude - city.longitude) / 2), 2)
                  ))))::integer <= preferences.radius_km
            GROUP BY agg.city_id
        ),
        supply AS (
            -- Confirmed bookable inventory per city: booking targets with a
            -- real route that are still on the board. Candidates awaiting
            -- screening do not count — "what's there" means what we could
            -- actually write to this week.
            SELECT
                agg.city_id,
                count(*) FILTER (WHERE target.target_kind = 'venue')::bigint AS venues,
                count(*) FILTER (WHERE target.target_kind = 'promoter')::bigint AS promoters,
                count(*) FILTER (WHERE target.target_kind = 'festival')::bigint AS festivals
            FROM agg
            JOIN viryaos_booking_targets AS target
              ON target.workspace_id = $1
             AND target.city_id = agg.city_id
             AND target.active
             AND target.accepts_booking
            GROUP BY agg.city_id
        ),
        shows AS (
            -- The gap edge: when we last played each city and when we next
            -- will. NULL last = never on record; NULL next = nothing
            -- coming — fans + no next show is the organise-now signal.
            SELECT
                agg.city_id,
                max(events.starts_at) FILTER (WHERE events.starts_at <= $2) AS last_show_at,
                min(events.starts_at) FILTER (WHERE events.starts_at > $2) AS next_show_at
            FROM agg
            JOIN events
              ON events.workspace_id = $1
             AND events.city_id = agg.city_id
             AND events.status IN ('published', 'completed')
            GROUP BY agg.city_id
        )
        SELECT
            agg.city_slug,
            agg.city_name,
            agg.country_code,
            agg.region,
            agg.fans,
            agg.new_30d,
            agg.active_30d,
            agg.consented,
            (agg.active_30d >= 50)::bool AS bookable,
            COALESCE(reach.reachable, 0)::bigint AS reachable,
            COALESCE(supply.venues, 0)::bigint AS venues,
            COALESCE(supply.promoters, 0)::bigint AS promoters,
            COALESCE(supply.festivals, 0)::bigint AS festivals,
            shows.last_show_at,
            shows.next_show_at
        FROM agg
        LEFT JOIN reachable AS reach
          ON reach.city_id = agg.city_id
        LEFT JOIN supply
          ON supply.city_id = agg.city_id
        LEFT JOIN shows
          ON shows.city_id = agg.city_id
        ORDER BY agg.active_30d DESC, agg.fans DESC, agg.city_slug, agg.city_id
        LIMIT 100
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(now)
    .fetch_all(&state.database)
    .await;
    let result = result.map(|mut rows| {
        let today = now.date();
        for row in &mut rows {
            row.months_since_show = row.last_show_at.map(|played| {
                let months = (today.year() - played.date().year()) * 12
                    + i32::from(u8::from(today.month()))
                    - i32::from(u8::from(played.date().month()));
                i64::from(months.max(0))
            });
            row.organise_score_bp = i64::from(crowdrelay_domain::place::organise_score(
                &crowdrelay_domain::place::OrganiseCityInputs {
                    reachable: row.reachable.max(0) as u32,
                    fans: row.fans.max(0) as u32,
                    new_30d: row.new_30d.max(0) as u32,
                    venues: row.venues.max(0) as u32,
                    promoters: row.promoters.max(0) as u32,
                    festivals: row.festivals.max(0) as u32,
                    months_since_show: row.months_since_show.map(|m| m.max(0) as u32),
                    next_show_booked: row.next_show_at.is_some(),
                },
            ));
        }
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
pub async fn city_venues(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let result = sqlx::query_as::<_, CityVenueRow>(
        r#"
        WITH marks AS (
            SELECT mark.venue_id, mark.event_id, mark.workspace_id,
                   event.starts_at, event.status
            FROM place_venue_marks AS mark
            JOIN events AS event
              ON event.id = mark.event_id
        ), per_show_draw AS (
            SELECT marks.venue_id, marks.event_id,
                   count(ticket_order.id)::double precision AS paid_orders
            FROM marks
            JOIN ticket_sales AS sale
              ON sale.workspace_id = marks.workspace_id
             AND sale.event_id = marks.event_id
            LEFT JOIN ticket_orders AS ticket_order
              ON ticket_order.workspace_id = sale.workspace_id
             AND ticket_order.ticket_sale_id = sale.id
             AND ticket_order.status IN ('paid', 'partially_refunded')
            GROUP BY marks.venue_id, marks.event_id
        ), repeaters AS (
            -- A repeat attender is a person who PAID for shows at the room
            -- at least twice — declared interest is not attendance, and the
            -- buyer's email is the only identity that survives across
            -- tenants. Two bands sharing a room's regulars is exactly the
            -- cross-tenant knowledge this registry exists to surface.
            SELECT marked.venue_id, count(*)::bigint AS repeat_attenders
            FROM (
                SELECT mark.venue_id, lower(btrim(ticket_order.buyer_email)) AS buyer
                FROM ticket_orders AS ticket_order
                JOIN ticket_sales AS sale
                  ON sale.workspace_id = ticket_order.workspace_id
                 AND sale.id = ticket_order.ticket_sale_id
                JOIN place_venue_marks AS mark
                  ON mark.event_id = sale.event_id
                 AND mark.workspace_id = sale.workspace_id
                WHERE ticket_order.status IN ('paid', 'partially_refunded')
                GROUP BY mark.venue_id, lower(btrim(ticket_order.buyer_email))
                HAVING count(DISTINCT sale.event_id) >= 2
            ) AS marked
            GROUP BY marked.venue_id
        ), resolved AS (
            -- The first non-expired fact per attribute in provenance trust
            -- order: played beats researched beats evidence beats a
            -- directory. workspace_id IS NULL is the load-bearing filter —
            -- this read is global, and a contributor's private fact (a
            -- booking address, a fit judgement) must never surface here.
            SELECT DISTINCT ON (f.venue_id, f.attribute)
                   f.venue_id, f.attribute, f.value, f.provenance, f.observed_at
            FROM place_venue_facts AS f
            WHERE (f.expires_at IS NULL OR f.expires_at > now())
              AND f.workspace_id IS NULL
            ORDER BY f.venue_id, f.attribute,
                     CASE f.provenance
                         WHEN 'played' THEN 0 WHEN 'researched' THEN 1
                         WHEN 'event_evidence' THEN 2 WHEN 'open_directory' THEN 3
                         ELSE 4 END,
                     f.observed_at DESC
        )
        SELECT
            venue.id AS venue_id,
            venue.display_name,
            city.slug AS city_slug,
            city.name AS city_name,
            city.country_code,
            -- Played is past tense — a booked future night is not a show
            -- the room has seen yet; it reads separately.
            count(marks.event_id) FILTER (
                WHERE marks.starts_at <= now()
            )::bigint AS shows_played,
            count(marks.event_id) FILTER (
                WHERE marks.starts_at > now() AND marks.status = 'published'
            )::bigint AS shows_booked,
            count(DISTINCT marks.workspace_id)::bigint AS contributors,
            avg(draw.paid_orders) AS typical_draw,
            COALESCE(repeaters.repeat_attenders, 0)::bigint AS repeat_attenders,
            max(marks.starts_at) FILTER (WHERE marks.starts_at <= now()) AS last_played_at,
            min(marks.starts_at) FILTER (
                WHERE marks.starts_at > now() AND marks.status = 'published'
            ) AS next_show_at,
            -- Each resolved join yields at most one row per venue —
            -- DISTINCT ON (venue_id, attribute) — so max() lifts the single
            -- surviving fact out of the aggregate rather than choosing
            -- between rival claims.
            max(cap.value) AS capacity_fact,
            max(cap.provenance) AS capacity_provenance,
            max(cap.observed_at) AS capacity_observed_at,
            max(gen.value) AS genres_fact,
            max(gen.provenance) AS genres_provenance,
            max(gen.observed_at) AS genres_observed_at,
            max(web.value) AS website_fact,
            max(web.provenance) AS website_provenance,
            max(web.observed_at) AS website_observed_at,
            max(addr.value) AS address_fact,
            max(addr.provenance) AS address_provenance,
            max(addr.observed_at) AS address_observed_at,
            max(stat.value) AS status_fact,
            max(stat.provenance) AS status_provenance,
            max(stat.observed_at) AS status_observed_at
        FROM place_venues AS venue
        JOIN cities AS city
          ON city.id = venue.city_id
        LEFT JOIN marks
          ON marks.venue_id = venue.id
        LEFT JOIN per_show_draw AS draw
          ON draw.venue_id = marks.venue_id
         AND draw.event_id = marks.event_id
        LEFT JOIN repeaters
          ON repeaters.venue_id = venue.id
        LEFT JOIN resolved AS cap
          ON cap.venue_id = venue.id AND cap.attribute = 'capacity'
        LEFT JOIN resolved AS gen
          ON gen.venue_id = venue.id AND gen.attribute = 'genres'
        LEFT JOIN resolved AS web
          ON web.venue_id = venue.id AND web.attribute = 'website'
        LEFT JOIN resolved AS addr
          ON addr.venue_id = venue.id AND addr.attribute = 'address'
        LEFT JOIN resolved AS stat
          ON stat.venue_id = venue.id AND stat.attribute = 'status'
        GROUP BY venue.id, venue.display_name, city.slug, city.name,
                 city.country_code, repeaters.repeat_attenders
        ORDER BY shows_played DESC, venue.display_name, venue.id
        LIMIT 500
        "#,
    )
    .fetch_all(&state.database)
    .await;
    private_json(result, &headers)
}

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
