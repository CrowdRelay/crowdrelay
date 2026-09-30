//! Internal registry rescan trigger for the n8n sync executor.
//!
//! The scout-registry n8n workflow merges new findings into the Google
//! Sheet, then asks CrowdRelay to re-read its sources now rather than at
//! the next scheduled sweep. This endpoint is the wake-up: one POST
//! notifies both sync workers over the same `pg_notify` channels their
//! boot and schedule loops already listen on —
//!
//! - `gdrive_contacts` → `GDriveContactsSyncWorker` scans every in-scope
//!   Drive workbook (SCOUT, SCOUT AUTO, MASTER, PROMO…) and runs the
//!   shared sheet intake;
//! - `github_registry` → `GithubRegistrySyncWorker` re-polls the
//!   configured registry repo (`crowdrelay-db`) for changed
//!   `.xlsx`/`.csv`/`.tsv` files and runs the same intake.
//!
//! The notify is fire-and-forget — if a worker is down the next periodic
//! sweep covers, and dedupe across re-runs is the intake's own conflict
//! keys, so a wake requested twice costs one extra no-change scan.
//!
//! `/v1/internal/` authenticates as the commerce key — the executor
//! surface n8n already holds. The route is deliberately only a wake: it
//! carries no payload, makes no decisions, and keeps the admin-only
//! `scan-now` control-plane routes out of the executor's reach.

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};

use crate::{Problem, request_id};

/// POST — wake both registry sync workers for an immediate scan.
pub async fn registry_sync_now(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    for (channel, payload) in [("gdrive_contacts", "scan"), ("github_registry", "sync")] {
        if let Err(error) = sqlx::query("SELECT pg_notify($1, $2)")
            .bind(channel)
            .bind(payload)
            .execute(&state.database)
            .await
        {
            tracing::warn!(%error, channel, "registry sync notify failed");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    }
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "scan": "requested" })),
    )
        .into_response()
}
