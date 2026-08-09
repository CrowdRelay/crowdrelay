//! Fan-facing read models that keep the Virya site and Virya Signal app on one
//! consistent snapshot. These endpoints are read-only and private; they never
//! expose raw fan-session, pass, wallet or Synesthesia run tokens.

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use sqlx::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{Problem, acquisition::fan_session_from_headers, request_id};

const PRIVATE_REVALIDATE: &str = "private, max-age=20, stale-if-error=600";
const PRIVATE_NO_STORE: &str = "private, no-store";
const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, FromRow)]
struct FanIdentity {
    id: Uuid,
    display_name: Option<String>,
    locale: Option<String>,
    normalized_email: String,
}

#[derive(Debug, Serialize, FromRow)]
pub struct FanHomeEvent {
    slug: String,
    title: String,
    venue: Option<String>,
    city: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    starts_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    doors_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    ends_at: Option<OffsetDateTime>,
    phase: String,
    ticket_url: Option<String>,
    interested: bool,
    has_pass: bool,
    has_paid_ticket: bool,
    ticket_sale_active: bool,
}

#[derive(Debug, Serialize, Default)]
pub struct FanHomeSynesthesia {
    started: bool,
    completed: bool,
    rooms_completed: i16,
    client_total_elapsed_ms: Option<i64>,
    #[serde(with = "time::serde::rfc3339::option")]
    linked_at: Option<OffsetDateTime>,
    reward_entered: bool,
}

#[derive(Debug, Serialize, Default)]
pub struct FanHomeReferral {
    qualified: i64,
    pending: i64,
}

#[derive(Debug, Serialize, Default)]
pub struct FanHomeCounts {
    event_interests: i64,
    active_passes: i64,
    paid_orders: i64,
    area_claims: i64,
}

#[derive(Debug, Serialize)]
pub struct FanHomeProfile {
    display_name: Option<String>,
    locale: Option<String>,
    primary_city: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct FanHomeResponse {
    schema_version: u32,
    #[serde(with = "time::serde::rfc3339")]
    generated_at: OffsetDateTime,
    profile: FanHomeProfile,
    next_event: Option<FanHomeEvent>,
    synesthesia: FanHomeSynesthesia,
    referral: FanHomeReferral,
    counts: FanHomeCounts,
    recommended_action: &'static str,
}

#[derive(Debug, Serialize, FromRow)]
pub struct FanEventContext {
    schema_version: i32,
    slug: String,
    title: String,
    venue: Option<String>,
    city: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    starts_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    doors_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    ends_at: Option<OffsetDateTime>,
    phase: String,
    ticket_url: Option<String>,
    interested: bool,
    pass_status: Option<String>,
    paid_ticket_quantity: i64,
    ticket_sale_active: bool,
    recommended_action: String,
}

#[derive(Debug, Serialize, FromRow)]
pub struct StaffEventDashboard {
    schema_version: i32,
    slug: String,
    title: String,
    venue: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    starts_at: OffsetDateTime,
    interested_fans: i64,
    paid_orders: i64,
    paid_tickets: i64,
    passes_issued: i64,
    passes_claimed: i64,
    passes_redeemed: i64,
}

#[derive(Debug, Clone, Copy)]
enum ContextError {
    Unauthorized,
    NotFound,
    Unavailable,
}

impl ContextError {
    fn response(self, request_id_value: Option<String>) -> Response {
        match self {
            Self::Unauthorized => Problem::unauthorized(request_id_value)
                .private()
                .into_response(),
            Self::NotFound => Problem::not_found(request_id_value)
                .private()
                .into_response(),
            Self::Unavailable => Problem::service_unavailable(request_id_value)
                .private()
                .into_response(),
        }
    }

    fn sqlx(error: sqlx::Error) -> Self {
        tracing::warn!(%error, "fan context read-model query failed");
        Self::Unavailable
    }
}

async fn current_fan(
    state: &crate::AppState,
    headers: &HeaderMap,
) -> Result<FanIdentity, ContextError> {
    let Some(session) = fan_session_from_headers(headers) else {
        return Err(ContextError::Unauthorized);
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    sqlx::query_as::<_, FanIdentity>(
        r#"
        WITH valid_session AS (
            SELECT session.workspace_id, session.fan_id
            FROM fan_sessions AS session
            INNER JOIN fans AS fan
              ON fan.workspace_id = session.workspace_id
             AND fan.id = session.fan_id
            WHERE session.workspace_id = $1
              AND session.session_token_hash = digest($2, 'sha256')
              AND session.revoked_at IS NULL
              AND session.expires_at > now()
              AND fan.status = 'active'
            LIMIT 1
        ), touched AS (
            UPDATE fan_sessions AS session
            SET last_seen_at = now()
            FROM valid_session AS valid
            WHERE session.workspace_id = valid.workspace_id
              AND session.fan_id = valid.fan_id
              AND session.session_token_hash = digest($2, 'sha256')
              AND session.last_seen_at < now() - interval '15 minutes'
            RETURNING session.fan_id
        )
        SELECT fan.id, fan.display_name, fan.locale, fan.normalized_email
        FROM valid_session AS valid
        INNER JOIN fans AS fan
          ON fan.workspace_id = valid.workspace_id
         AND fan.id = valid.fan_id
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .bind(session.as_str())
    .fetch_optional(state.ticketing.pool())
    .await
    .map_err(ContextError::sqlx)?
    .ok_or(ContextError::Unauthorized)
}

pub async fn fan_home(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    let fan = match current_fan(&state, &headers).await {
        Ok(fan) => fan,
        Err(error) => return error.response(request_id_value),
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();

    let primary_city = sqlx::query_scalar::<_, String>(
        r#"
        SELECT city.name
        FROM fan_city_interests AS interest
        INNER JOIN cities AS city ON city.id = interest.city_id
        WHERE interest.workspace_id = $1 AND interest.fan_id = $2
        ORDER BY interest.created_at, city.id
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .bind(fan.id)
    .fetch_optional(state.ticketing.pool());

    let next_event = sqlx::query_as::<_, FanHomeEvent>(
        r#"
        SELECT event.slug, event.title, event.venue, city.name AS city,
               event.starts_at, event.doors_at, event.ends_at,
               CASE
                 WHEN event.starts_at <= now()
                  AND COALESCE(event.ends_at, event.starts_at + interval '4 hours') >= now() THEN 'live'
                 WHEN COALESCE(event.ends_at, event.starts_at + interval '4 hours') < now() THEN 'afterglow'
                 ELSE 'upcoming'
               END AS phase,
               event.ticket_url,
               EXISTS(
                   SELECT 1 FROM event_interests AS interest
                   WHERE interest.workspace_id = event.workspace_id
                     AND interest.event_id = event.id AND interest.fan_id = $2
               ) AS interested,
               EXISTS(
                   SELECT 1 FROM admission_passes AS pass
                   WHERE pass.workspace_id = event.workspace_id
                     AND pass.event_id = event.id AND pass.fan_id = $2
                     AND pass.status IN ('issued', 'claimed')
               ) AS has_pass,
               EXISTS(
                   SELECT 1
                   FROM ticket_orders AS order_row
                   INNER JOIN ticket_sales AS paid_sale
                     ON paid_sale.workspace_id = order_row.workspace_id
                    AND paid_sale.id = order_row.ticket_sale_id
                   WHERE order_row.workspace_id = event.workspace_id
                     AND paid_sale.event_id = event.id
                     AND order_row.buyer_email = $3
                     AND order_row.status IN ('paid', 'partially_refunded')
               ) AS has_paid_ticket,
               EXISTS(
                   SELECT 1 FROM ticket_sales AS sale
                   WHERE sale.workspace_id = event.workspace_id
                     AND sale.event_id = event.id AND sale.active
                     AND sale.sales_open_at <= now() AND sale.sales_close_at > now()
               ) AS ticket_sale_active
        FROM events AS event
        LEFT JOIN cities AS city ON city.id = event.city_id
        WHERE event.workspace_id = $1
          AND event.status = 'published'
          AND COALESCE(event.ends_at, event.starts_at + interval '4 hours') > now() - interval '12 hours'
        ORDER BY
          CASE
            WHEN event.starts_at <= now()
             AND COALESCE(event.ends_at, event.starts_at + interval '4 hours') >= now() THEN 0
            WHEN COALESCE(event.ends_at, event.starts_at + interval '4 hours') < now() THEN 1
            ELSE 2
          END,
          CASE WHEN EXISTS(
              SELECT 1 FROM fan_city_interests AS preferred
              WHERE preferred.workspace_id = event.workspace_id
                AND preferred.fan_id = $2 AND preferred.city_id = event.city_id
          ) THEN 0 ELSE 1 END,
          ABS(EXTRACT(EPOCH FROM (event.starts_at - now()))), event.id
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .bind(fan.id)
    .bind(&fan.normalized_email)
    .fetch_optional(state.ticketing.pool());

    let synesthesia = sqlx::query_as::<
        _,
        (
            i16,
            Option<OffsetDateTime>,
            Option<i64>,
            Option<OffsetDateTime>,
            bool,
        ),
    >(
        r#"
        SELECT run.next_room_index, run.completed_at, run.client_total_elapsed_ms, run.linked_at,
               EXISTS(
                   SELECT 1 FROM synesthesia_reward_entries AS reward
                   WHERE reward.workspace_id = run.workspace_id AND reward.run_id = run.id
               ) AS reward_entered
        FROM synesthesia_runs AS run
        WHERE run.workspace_id = $1 AND run.fan_id = $2
        ORDER BY run.completed_at DESC NULLS LAST, run.updated_at DESC, run.id DESC
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .bind(fan.id)
    .fetch_optional(state.ticketing.pool());

    let referral = sqlx::query_as::<_, (i64, i64)>(
        r#"
        SELECT
          COUNT(*) FILTER (WHERE status = 'qualified')::bigint AS qualified,
          COUNT(*) FILTER (WHERE status = 'pending')::bigint AS pending
        FROM referral_attributions
        WHERE workspace_id = $1 AND referrer_fan_id = $2
        "#,
    )
    .bind(workspace_id)
    .bind(fan.id)
    .fetch_one(state.ticketing.pool());

    let counts = sqlx::query_as::<_, (i64, i64, i64, i64)>(
        r#"
        SELECT
          (SELECT COUNT(*)::bigint FROM event_interests
             WHERE workspace_id = $1 AND fan_id = $2) AS event_interests,
          (SELECT COUNT(*)::bigint FROM admission_passes
             WHERE workspace_id = $1 AND fan_id = $2 AND status IN ('issued', 'claimed')) AS active_passes,
          (SELECT COUNT(*)::bigint FROM ticket_orders
             WHERE workspace_id = $1 AND buyer_email = $3
               AND status IN ('paid', 'partially_refunded')) AS paid_orders,
          (SELECT COUNT(*)::bigint
             FROM area_claims AS claim
             INNER JOIN area_players AS player
               ON player.workspace_id = claim.workspace_id AND player.id = claim.player_id
             WHERE claim.workspace_id = $1 AND player.fan_id = $2) AS area_claims
        "#,
    )
    .bind(workspace_id)
    .bind(fan.id)
    .bind(&fan.normalized_email)
    .fetch_one(state.ticketing.pool());

    let (primary_city, next_event, synesthesia, referral, counts) =
        match tokio::try_join!(primary_city, next_event, synesthesia, referral, counts,) {
            Ok(values) => values,
            Err(error) => return ContextError::sqlx(error).response(request_id_value),
        };

    let synesthesia = match synesthesia {
        Some((rooms, completed_at, elapsed, linked_at, reward_entered)) => FanHomeSynesthesia {
            started: true,
            completed: completed_at.is_some(),
            rooms_completed: rooms,
            client_total_elapsed_ms: elapsed,
            linked_at,
            reward_entered,
        },
        None => FanHomeSynesthesia::default(),
    };
    let referral = FanHomeReferral {
        qualified: referral.0,
        pending: referral.1,
    };
    let counts = FanHomeCounts {
        event_interests: counts.0,
        active_passes: counts.1,
        paid_orders: counts.2,
        area_claims: counts.3,
    };
    let recommended_action = match next_event.as_ref() {
        Some(event) if event.phase == "live" && (event.has_pass || event.has_paid_ticket) => {
            "open_wallet"
        }
        Some(event) if event.phase == "live" => "open_live_event",
        Some(event) if event.phase == "afterglow" => "share_post_show_feedback",
        Some(event) if event.has_pass || event.has_paid_ticket => "open_wallet",
        Some(event) if event.ticket_sale_active => "get_ticket",
        Some(event) if !event.interested => "follow_next_event",
        _ if !synesthesia.completed => "continue_synesthesia",
        _ => "explore_signal",
    };

    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_REVALIDATE)],
        Json(FanHomeResponse {
            schema_version: SCHEMA_VERSION,
            generated_at: OffsetDateTime::now_utc(),
            profile: FanHomeProfile {
                display_name: fan.display_name,
                locale: fan.locale,
                primary_city,
            },
            next_event,
            synesthesia,
            referral,
            counts,
            recommended_action,
        }),
    )
        .into_response()
}

pub async fn fan_event_context(
    State(state): State<crate::AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let fan = match current_fan(&state, &headers).await {
        Ok(fan) => fan,
        Err(error) => return error.response(request_id_value),
    };
    if slug.is_empty() || slug.len() > 128 {
        return Problem::not_found(request_id_value)
            .private()
            .into_response();
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let context = sqlx::query_as::<_, FanEventContext>(
        r#"
        SELECT $4::integer AS schema_version,
               event.slug, event.title, event.venue, city.name AS city,
               event.starts_at, event.doors_at, event.ends_at,
               CASE
                 WHEN event.starts_at <= now()
                  AND COALESCE(event.ends_at, event.starts_at + interval '4 hours') >= now() THEN 'live'
                 WHEN COALESCE(event.ends_at, event.starts_at + interval '4 hours') < now() THEN 'afterglow'
                 ELSE 'upcoming'
               END AS phase,
               event.ticket_url,
               EXISTS(
                 SELECT 1 FROM event_interests AS interest
                 WHERE interest.workspace_id = event.workspace_id
                   AND interest.event_id = event.id AND interest.fan_id = $2
               ) AS interested,
               (
                 SELECT pass.status FROM admission_passes AS pass
                 WHERE pass.workspace_id = event.workspace_id
                   AND pass.event_id = event.id AND pass.fan_id = $2
                 ORDER BY pass.updated_at DESC, pass.id DESC LIMIT 1
               ) AS pass_status,
               COALESCE((
                 SELECT SUM(item.quantity)::bigint
                 FROM ticket_orders AS order_row
                 INNER JOIN ticket_sales AS sale
                   ON sale.workspace_id = order_row.workspace_id AND sale.id = order_row.ticket_sale_id
                 INNER JOIN ticket_order_items AS item
                   ON item.workspace_id = order_row.workspace_id AND item.ticket_order_id = order_row.id
                 WHERE order_row.workspace_id = event.workspace_id
                   AND sale.event_id = event.id
                   AND order_row.buyer_email = $5
                   AND order_row.status IN ('paid', 'partially_refunded')
               ), 0)::bigint AS paid_ticket_quantity,
               EXISTS(
                 SELECT 1 FROM ticket_sales AS sale
                 WHERE sale.workspace_id = event.workspace_id AND sale.event_id = event.id
                   AND sale.active AND sale.sales_open_at <= now() AND sale.sales_close_at > now()
               ) AS ticket_sale_active,
               CASE
                 WHEN event.starts_at <= now()
                  AND COALESCE(event.ends_at, event.starts_at + interval '4 hours') >= now()
                  AND (
                    EXISTS(SELECT 1 FROM admission_passes AS p WHERE p.workspace_id = event.workspace_id AND p.event_id = event.id AND p.fan_id = $2 AND p.status IN ('issued','claimed'))
                    OR EXISTS(
                      SELECT 1 FROM ticket_orders AS o
                      INNER JOIN ticket_sales AS s ON s.workspace_id=o.workspace_id AND s.id=o.ticket_sale_id
                      WHERE o.workspace_id=event.workspace_id AND s.event_id=event.id AND o.buyer_email=$5 AND o.status IN ('paid','partially_refunded')
                    )
                  ) THEN 'open_wallet'
                 WHEN event.starts_at <= now()
                  AND COALESCE(event.ends_at, event.starts_at + interval '4 hours') >= now() THEN 'open_live_event'
                 WHEN COALESCE(event.ends_at, event.starts_at + interval '4 hours') < now() THEN 'share_post_show_feedback'
                 WHEN EXISTS(SELECT 1 FROM ticket_sales AS s WHERE s.workspace_id=event.workspace_id AND s.event_id=event.id AND s.active AND s.sales_open_at <= now() AND s.sales_close_at > now()) THEN 'get_ticket'
                 ELSE 'follow_event'
               END AS recommended_action
        FROM events AS event
        LEFT JOIN cities AS city ON city.id = event.city_id
        WHERE event.workspace_id = $1 AND event.slug = $3 AND event.status = 'published'
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .bind(fan.id)
    .bind(slug)
    .bind(i32::try_from(SCHEMA_VERSION).unwrap_or(1))
    .bind(fan.normalized_email)
    .fetch_optional(state.ticketing.pool())
    .await;

    match context {
        Ok(Some(context)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_REVALIDATE)],
            Json(context),
        )
            .into_response(),
        Ok(None) => ContextError::NotFound.response(request_id_value),
        Err(error) => ContextError::sqlx(error).response(request_id_value),
    }
}

pub async fn staff_event_dashboard(
    State(state): State<crate::AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    if !state.ticketing.operator_authorized(&headers) {
        return ContextError::Unauthorized.response(request_id_value);
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let dashboard = sqlx::query_as::<_, StaffEventDashboard>(
        r#"
        SELECT $3::integer AS schema_version,
               event.slug, event.title, event.venue, event.starts_at,
               (SELECT COUNT(*)::bigint FROM event_interests AS interest
                 WHERE interest.workspace_id = event.workspace_id AND interest.event_id = event.id) AS interested_fans,
               (SELECT COUNT(*)::bigint FROM ticket_orders AS order_row
                 INNER JOIN ticket_sales AS sale
                   ON sale.workspace_id = order_row.workspace_id AND sale.id = order_row.ticket_sale_id
                 WHERE order_row.workspace_id = event.workspace_id AND sale.event_id = event.id
                   AND order_row.status IN ('paid', 'partially_refunded')) AS paid_orders,
               COALESCE((SELECT SUM(item.quantity)::bigint FROM ticket_order_items AS item
                 INNER JOIN ticket_orders AS order_row
                   ON order_row.workspace_id = item.workspace_id AND order_row.id = item.ticket_order_id
                 INNER JOIN ticket_sales AS sale
                   ON sale.workspace_id = order_row.workspace_id AND sale.id = order_row.ticket_sale_id
                 WHERE order_row.workspace_id = event.workspace_id AND sale.event_id = event.id
                   AND order_row.status IN ('paid', 'partially_refunded')), 0)::bigint AS paid_tickets,
               (SELECT COUNT(*)::bigint FROM admission_passes AS pass
                 WHERE pass.workspace_id = event.workspace_id AND pass.event_id = event.id) AS passes_issued,
               (SELECT COUNT(*)::bigint FROM admission_passes AS pass
                 WHERE pass.workspace_id = event.workspace_id AND pass.event_id = event.id
                   AND pass.status = 'claimed') AS passes_claimed,
               (SELECT COUNT(*)::bigint FROM admission_passes AS pass
                 WHERE pass.workspace_id = event.workspace_id AND pass.event_id = event.id
                   AND pass.status = 'redeemed') AS passes_redeemed
        FROM events AS event
        WHERE event.workspace_id = $1 AND event.slug = $2
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .bind(slug)
    .bind(i32::try_from(SCHEMA_VERSION).unwrap_or(1))
    .fetch_optional(state.ticketing.pool())
    .await;

    match dashboard {
        Ok(Some(dashboard)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(dashboard),
        )
            .into_response(),
        Ok(None) => ContextError::NotFound.response(request_id_value),
        Err(error) => ContextError::sqlx(error).response(request_id_value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_cache_is_short_and_private() {
        assert!(PRIVATE_REVALIDATE.contains("private"));
        assert!(PRIVATE_REVALIDATE.contains("stale-if-error=600"));
        assert_eq!(SCHEMA_VERSION, 1);
    }
}
