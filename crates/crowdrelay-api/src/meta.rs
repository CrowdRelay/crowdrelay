//! Stable, public compatibility contract for independently deployed clients.

use axum::{Json, http::header::CACHE_CONTROL, response::IntoResponse};
use serde::Serialize;
use std::collections::BTreeMap;

const API_VERSION: &str = "1";
pub(crate) const SCHEMA_VERSION: u32 = 66;
const CACHE: &str = "public, max-age=30, s-maxage=30, stale-while-revalidate=60";

pub(crate) fn git_sha() -> Option<&'static str> {
    option_env!("CROWDRELAY_GIT_SHA").filter(|value| !value.is_empty())
}

pub(crate) fn release_identity() -> &'static str {
    git_sha()
        .or(option_env!("CROWDRELAY_RELEASE").filter(|value| !value.is_empty()))
        .unwrap_or(env!("CARGO_PKG_VERSION"))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MetaResponse {
    api_version: &'static str,
    schema_version: u32,
    release: &'static str,
    git_sha: Option<&'static str>,
    build_timestamp: Option<&'static str>,
    minimum_postgres_server_version_num: i32,
    capabilities: BTreeMap<&'static str, bool>,
}

pub async fn get() -> impl IntoResponse {
    let capabilities = BTreeMap::from([
        ("area_claims_v1", true),
        ("area_wallet_postgres_v2", true),
        ("area_vouchers_v2", true),
        ("area_ticket_rewards_v2", true),
        ("signal_fan_context_v1", true),
        ("signal_wallet_v1", true),
        ("synesthesia_runs_v1", true),
        ("synesthesia_rewards_v1", true),
        ("synesthesia_leaderboard_v1", true),
        ("synesthesia_recovery_v1", true),
        ("ticketing_v1", true),
        ("staff_device_sessions_v2", true),
        ("viryaos_ops_v1", true),
        ("viryaos_beacons_v1", true),
        ("beacon_signal_v1", true),
        ("beacon_signal_v2", true),
        ("beacon_native_signal_v1", true),
        ("beacon_physical_releases_v1", true),
        ("beacon_network_acquisition_v1", true),
        ("viryaos_team_handoffs_v1", true),
        ("viryaos_show_growth_v1", true),
        ("communication_delivery_ledger_v1", true),
        ("fan_push_delivery_v1", true),
        ("staff_show_checklist_push_v1", true),
    ]);
    (
        [(CACHE_CONTROL, CACHE)],
        Json(MetaResponse {
            api_version: API_VERSION,
            schema_version: SCHEMA_VERSION,
            release: option_env!("CROWDRELAY_RELEASE")
                .filter(|value| !value.is_empty())
                .unwrap_or(env!("CARGO_PKG_VERSION")),
            git_sha: git_sha(),
            build_timestamp: option_env!("CROWDRELAY_BUILD_TIMESTAMP")
                .filter(|value| !value.is_empty()),
            minimum_postgres_server_version_num:
                crowdrelay_infra::database::MIN_POSTGRES_SERVER_VERSION_NUM,
            capabilities,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatibility_contract_tracks_latest_migration() {
        assert_eq!(SCHEMA_VERSION, 66);
    }
}
