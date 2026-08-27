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
/// have crossed the minimum — 50 active fans by default, configurable.
pub async fn city_funnel(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let now = OffsetDateTime::now_utc();
    let result = sqlx::query_as::<_, CityFunnelRow>(
        r#"
        WITH city_fans AS (
            SELECT
                city.id AS city_id,
                city.slug AS city_slug,
                city.name AS city_name,
                city.country_code,
                fan.id AS fan_id,
                fan.normalized_email,
                fan.last_activity_at,
                EXISTS (
                    SELECT 1 FROM fan_consents AS consent
                    WHERE consent.workspace_id = fan.workspace_id
                      AND consent.fan_id = fan.id
                      AND consent.purpose = 'marketing'
                      AND consent.granted
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
              AND fan.status <> 'closed'
        )
        SELECT
            city_slug,
            city_name,
            country_code,
            count(*)::bigint AS fans,
            count(*) FILTER (
                WHERE last_meaningful_action_at IS NOT NULL
                  AND last_meaningful_action_at BETWEEN $2 - INTERVAL '30 days' AND $2
                  AND consented
            )::bigint AS active_30d,
            count(*) FILTER (WHERE consented)::bigint AS consented,
            (count(*) FILTER (
                WHERE last_meaningful_action_at IS NOT NULL
                  AND last_meaningful_action_at BETWEEN $2 - INTERVAL '30 days' AND $2
                  AND consented
            ) >= 50)::bool AS bookable
        FROM city_fans
        GROUP BY city_slug, city_name, country_code
        ORDER BY active_30d DESC, fans DESC, city_slug
        LIMIT 100
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(now)
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
