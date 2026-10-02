//! The demand scout's read surface: which strangers, right now, in rooms the
//! band reads, are asking for music like the band's.
//!
//! Read-only and control-plane-authed (see `community_intelligence_routes` for
//! why the prefix matters). The statements live in `crowdrelay-infra::fan_demand`
//! and the rules in `crowdrelay-domain::fan_demand`; this file only maps
//! between them and HTTP. It drafts nothing and posts nothing.

use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
    routing::get,
};
use crowdrelay_domain::fan_demand::{
    DemandKind, MAX_AGE_DAYS, ThreadFacts, classify, reach_basis_points, score_basis_points,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";
/// Threads read per request. The sweep records hundreds a day across the
/// joined rooms; the newest few thousand cover the whole three-day window.
const SCAN_LIMIT: i64 = 5_000;
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 100;

pub(super) fn control_plane_routes() -> axum::Router<crate::AppState> {
    axum::Router::new().route("/v1/control-plane/growth/demand-threads", get(list))
}

#[derive(Deserialize)]
struct ListQuery {
    limit: Option<usize>,
}

#[derive(Serialize)]
struct DemandThread {
    url: String,
    room: String,
    title: String,
    kind: DemandKind,
    /// The classifier rule that surfaced the thread, so a wrong one is visible.
    rule: &'static str,
    comments: Option<u32>,
    age_days: i64,
    reach_basis_points: u16,
    score_basis_points: u16,
}

/// `GET /v1/control-plane/growth/demand-threads` — threads that ask for music,
/// best first. `scanned` is how many recent threads were read, so an empty
/// `threads` list is distinguishable from an empty sweep.
async fn list(
    State(state): State<crate::AppState>,
    Query(query): Query<ListQuery>,
    headers: HeaderMap,
) -> Response {
    let now = OffsetDateTime::now_utc();
    let rows = match crowdrelay_infra::fan_demand::recent_room_threads(
        &state.database,
        state.ticketing.workspace_id().into_uuid(),
        now.date() - time::Duration::days(MAX_AGE_DAYS),
        SCAN_LIMIT,
    )
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "demand thread read failed");
            return Problem::service_unavailable(request_id(&headers)).into_response();
        }
    };
    let scanned = rows.len();
    let mut threads: Vec<DemandThread> = rows
        .iter()
        .filter_map(|row| {
            let facts = ThreadFacts {
                title: &row.title,
                flair: row.flair.as_deref(),
                comments: row.comments.and_then(|c| u32::try_from(c).ok()),
                posted_on: row.posted_on,
            };
            let signal = classify(&facts)?;
            let score = score_basis_points(&facts, now);
            (score > 0).then(|| DemandThread {
                url: row.url.clone(),
                room: row.room.clone(),
                title: row.title.clone(),
                kind: signal.kind,
                rule: signal.rule,
                comments: facts.comments,
                age_days: (now.date() - row.posted_on).whole_days(),
                reach_basis_points: reach_basis_points(&facts, now),
                score_basis_points: score,
            })
        })
        .collect();
    threads.sort_by(|a, b| {
        b.score_basis_points
            .cmp(&a.score_basis_points)
            .then_with(|| a.url.cmp(&b.url))
    });
    let matched = threads.len();
    threads.truncate(query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT));
    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(serde_json::json!({
            "scanned": scanned,
            "matched": matched,
            "threads": threads,
        })),
    )
        .into_response()
}
