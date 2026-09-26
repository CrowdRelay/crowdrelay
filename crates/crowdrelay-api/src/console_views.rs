//! Console view read models — one read per console page, shaped to the page.
//!
//! The console used to open a city with five reads (the whole funnel, all
//! 163 rooms, the whole gig plan, every show, the staged contacts) and filter
//! them in the browser; the material page downloaded every source with its
//! full send trail to count four numbers. These routes answer the first
//! screen of a page in one request. What sits behind a page's tabs is not
//! here: tabs fetch their own reads when they are opened.
//!
//! Each view reuses the query its data already comes from, narrowed, rather
//! than re-deriving it — a city's fans come from the funnel query with a city
//! filter, its rooms from the registry query with a city filter, its verdict
//! from the planner. A second derivation of the same number is a number that
//! will one day disagree with the first.
//!
//! What is not one SQL statement, and why: the city view runs the funnel
//! row, the room list and the shows here as three statements, concurrently,
//! plus the planner's own per-city evidence reads. The planner is domain code
//! (`plan_gig`) over evidence, not a query, and folding the funnel's consent
//! and radius logic into a fourth copy was the alternative. The material view
//! is one statement.

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use time::OffsetDateTime;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";

fn private_ok<T: Serialize>(value: T) -> Response {
    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(value),
    )
        .into_response()
}

fn unavailable(headers: &HeaderMap, error: &sqlx::Error, view: &'static str) -> Response {
    tracing::warn!(%error, view, "console view read failed");
    Problem::service_unavailable(request_id(headers))
        .private()
        .into_response()
}

/// Shows in one city, newest first, and the numbers the last one left
/// behind. Venue names come from the room registry mark when one exists:
/// the event row's own `venue` column has carried a tour name or the title
/// itself, which is what the page used to print.
///
/// Every count that needs an instrument to exist is null without it: paid
/// buyers without a ticket sale, check-ins without a door campaign. A night
/// nobody measured at the door is "not measured", not "nobody came".
const CITY_SHOWS_SQL: &str = r#"
WITH shows AS (
    SELECT event.id, event.slug, event.title, event.starts_at, event.status,
           COALESCE(room.display_name, event.venue) AS venue
    FROM events AS event
    JOIN cities AS city
      ON city.id = event.city_id
     AND city.slug = $2
    LEFT JOIN LATERAL (
        SELECT venue.display_name
        FROM place_venue_marks AS mark
        JOIN place_venues AS venue ON venue.id = mark.venue_id
        WHERE mark.workspace_id = event.workspace_id
          AND mark.event_id = event.id
        ORDER BY venue.display_name
        LIMIT 1
    ) AS room ON true
    WHERE event.workspace_id = $1
      AND event.status IN ('published', 'completed')
), last_show AS (
    SELECT shows.*,
           EXISTS (
               SELECT 1 FROM ticket_sales AS sale
               WHERE sale.workspace_id = $1 AND sale.event_id = shows.id
           ) AS ticketed,
           (SELECT count(*) FROM concert_qr_campaigns AS campaign
            WHERE campaign.workspace_id = $1
              AND campaign.event_id = shows.id)::bigint AS door_campaigns
    FROM shows
    WHERE shows.starts_at <= $3
    ORDER BY shows.starts_at DESC
    LIMIT 1
)
SELECT jsonb_build_object(
    'shows', COALESCE((
        SELECT jsonb_agg(jsonb_build_object(
                   'slug', shows.slug,
                   'title', shows.title,
                   'venue', shows.venue,
                   'starts_at', shows.starts_at,
                   'status', shows.status
               ) ORDER BY shows.starts_at DESC)
        FROM shows
    ), '[]'::jsonb),
    'last_show', (
        SELECT jsonb_build_object(
            'slug', last.slug,
            'title', last.title,
            'venue', last.venue,
            'starts_at', last.starts_at,
            'paid_buyers', CASE WHEN last.ticketed THEN (
                SELECT count(DISTINCT lower(ticket_order.buyer_email))
                FROM ticket_orders AS ticket_order
                JOIN ticket_sales AS sale
                  ON sale.workspace_id = ticket_order.workspace_id
                 AND sale.id = ticket_order.ticket_sale_id
                WHERE ticket_order.workspace_id = $1
                  AND sale.event_id = last.id
                  AND ticket_order.status IN ('paid', 'partially_refunded')
            ) END,
            'ticket_clicks', (
                SELECT count(*) FROM event_action_events AS action
                WHERE action.workspace_id = $1
                  AND action.event_id = last.id
                  AND action.action = 'ticket_click'
            ),
            'interested', (
                SELECT count(*) FROM event_interests AS interest
                WHERE interest.workspace_id = $1 AND interest.event_id = last.id
            ),
            'checkins', CASE WHEN last.door_campaigns > 0 THEN (
                SELECT count(*) FROM concert_checkins AS checkin
                WHERE checkin.workspace_id = $1 AND checkin.event_id = last.id
            ) END
        )
        FROM last_show AS last
    )
)
"#;

/// `GET /v1/control-plane/views/cities/{city_slug}` — one city's first
/// screen: its funnel row, its rooms, the planner's verdict, the shows
/// played there and what the last one left behind.
///
/// `funnel` is null when no fan has named the city, `verdict` is null when
/// the planner did not consider it. Both are answers, not failures; the page
/// says which. Any read failing fails the view — a city page that silently
/// lost its rooms would read as a city with no rooms.
pub async fn city_view(
    State(state): State<crate::AppState>,
    Path(city_slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    let pool = &state.database;
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let now = OffsetDateTime::now_utc();
    let reads = crate::ops::hold(&state.read_budget, async {
        tokio::try_join!(
            crate::audience::city_funnel_rows(pool, workspace_id, now, Some(&city_slug)),
            crate::audience::city_venue_rows(&state, Some(&city_slug)),
            crate::gig_planning::city_verdict(pool, workspace_id, now, &city_slug),
            sqlx::query_scalar::<_, serde_json::Value>(CITY_SHOWS_SQL)
                .bind(workspace_id)
                .bind(&city_slug)
                .bind(now)
                .fetch_one(pool),
        )
    })
    .await;
    match reads {
        Ok((funnel, rooms, verdict, shows)) => private_ok(serde_json::json!({
            "city_slug": city_slug,
            "funnel": funnel.into_iter().next(),
            "rooms": rooms,
            "verdict": verdict,
            "shows": shows.get("shows").cloned().unwrap_or_else(|| serde_json::json!([])),
            "last_show": shows.get("last_show").cloned().unwrap_or(serde_json::Value::Null),
        })),
        Err(error) => unavailable(&headers, &error, "city"),
    }
}

/// The material page in one statement. "Usable" is `active` and not past
/// `expires_at` — the `active` flag alone stays true on a source long after
/// it aged out, which made 128 of 129 sources read as live. A use is a
/// content-supply action taken on the source.
///
/// `distinct_titles` counts a release once per song rather than once per
/// single, EP and album copy: the title is folded to lower case with a
/// leading "NN - Artist - " track prefix and a trailing "(…)" edition note
/// removed. The fold is a heuristic and the page labels it as "about".
const MATERIAL_SQL: &str = r#"
WITH uses AS (
    SELECT action.subject_id AS source_id,
           count(*)::bigint AS uses,
           max(action.created_at) AS last_used_at
    FROM autopilot_actions AS action
    WHERE action.workspace_id = $1
      AND action.context = 'content_supply'
      AND action.subject_id IS NOT NULL
    GROUP BY action.subject_id
), source AS (
    SELECT content.id, content.source_kind, content.source_key, content.title,
           content.occurred_at, content.expires_at,
           (content.active AND content.expires_at > $2) AS usable,
           COALESCE(uses.uses, 0)::bigint AS uses,
           uses.last_used_at,
           lower(btrim(regexp_replace(
               regexp_replace(content.title, '^\s*\d+\s*-\s*[^-]*-\s*', ''),
               '\s*\([^)]*\)\s*$', ''
           ))) AS folded_title
    FROM content_sources AS content
    LEFT JOIN uses ON uses.source_id = content.id
    WHERE content.workspace_id = $1
)
SELECT jsonb_build_object(
    'total', (SELECT count(*) FROM source),
    'usable', (SELECT count(*) FILTER (WHERE usable) FROM source),
    'used', (SELECT count(*) FILTER (WHERE uses > 0) FROM source),
    'uses_total', (SELECT COALESCE(sum(uses), 0)::bigint FROM source),
    'newest', (
        SELECT jsonb_build_object(
            'source_id', source.id,
            'kind', source.source_kind,
            'platform', split_part(source.source_key, ':', 1),
            'title', source.title,
            'occurred_at', source.occurred_at
        )
        FROM source
        ORDER BY source.occurred_at DESC, source.id
        LIMIT 1
    ),
    'by_kind', COALESCE((
        SELECT jsonb_agg(kind_row ORDER BY kind_row->>'kind')
        FROM (
            SELECT jsonb_build_object(
                'kind', source.source_kind,
                'total', count(*),
                'usable', count(*) FILTER (WHERE source.usable),
                'used', count(*) FILTER (WHERE source.uses > 0),
                'uses', COALESCE(sum(source.uses), 0)::bigint,
                'distinct_titles', count(DISTINCT source.folded_title),
                'oldest_unused_at', min(source.occurred_at) FILTER (WHERE source.uses = 0),
                'newest_unused_at', max(source.occurred_at) FILTER (WHERE source.uses = 0)
            ) AS kind_row
            FROM source
            GROUP BY source.source_kind
        ) AS kinds
    ), '[]'::jsonb),
    'recent', COALESCE((
        SELECT jsonb_agg(jsonb_build_object(
                   'source_id', recent.id,
                   'kind', recent.source_kind,
                   'platform', split_part(recent.source_key, ':', 1),
                   'title', recent.title,
                   'occurred_at', recent.occurred_at,
                   'expires_at', recent.expires_at,
                   'usable', recent.usable,
                   'uses', recent.uses,
                   'last_used_at', recent.last_used_at
               ) ORDER BY recent.occurred_at DESC, recent.id)
        FROM (
            SELECT * FROM source
            ORDER BY source.occurred_at DESC, source.id
            LIMIT 5
        ) AS recent
    ), '[]'::jsonb)
)
"#;

/// `GET /v1/control-plane/views/content-material` — the material page's
/// first screen. The full list with each source's send trail stays on
/// `autopilot/content-sources`, which the page's list tab reads when opened.
pub async fn content_material_view(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let read = crate::ops::hold(
        &state.read_budget,
        sqlx::query_scalar::<_, serde_json::Value>(MATERIAL_SQL)
            .bind(workspace_id)
            .bind(OffsetDateTime::now_utc())
            .fetch_one(&state.database),
    )
    .await;
    match read {
        Ok(view) => private_ok(view),
        Err(error) => unavailable(&headers, &error, "content_material"),
    }
}
