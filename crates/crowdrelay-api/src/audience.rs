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

include!("audience/query_support.rs");
