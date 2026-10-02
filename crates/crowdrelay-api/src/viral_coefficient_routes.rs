//! K, the one number that says whether the person layer is working.
//!
//! Read-only, control-plane-authed. Every series carries the counts behind it,
//! and K is `null` — with a reason — below its evidence floor: an unknown is not
//! reported as a small number.

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
    routing::get,
};
use crowdrelay_domain::viral_coefficient::{self, MIN_COHORT};
use serde_json::json;
use time::OffsetDateTime;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";

pub(super) fn control_plane_routes() -> axum::Router<crate::AppState> {
    axum::Router::new().route("/v1/control-plane/growth/viral-coefficient", get(read))
}

async fn read(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let now = OffsetDateTime::now_utc();
    match crowdrelay_infra::viral_coefficient::k_counts(
        &state.database,
        state.ticketing.workspace_id().into_uuid(),
        now,
    )
    .await
    {
        Ok(series) => {
            let fans = viral_coefficient::read(series.fans);
            let latarnik = viral_coefficient::read(series.latarnik);
            (
                StatusCode::OK,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(json!({
                    "min_cohort": MIN_COHORT,
                    // The windows, so the number is never read as "now".
                    "windows": {
                        "cohort_active": "90d..60d ago",
                        "referrals_qualified": "60d..30d ago",
                        "retained_active": "last 30d",
                    },
                    "fans": {
                        "reading": fans,
                        "self_sustaining": viral_coefficient::self_sustaining(&fans),
                    },
                    "latarnik": {
                        "reading": latarnik,
                        "self_sustaining": viral_coefficient::self_sustaining(&latarnik),
                    },
                })),
            )
                .into_response()
        }
        Err(error) => {
            tracing::warn!(%error, "viral coefficient read failed");
            Problem::service_unavailable(request_id(&headers)).into_response()
        }
    }
}
