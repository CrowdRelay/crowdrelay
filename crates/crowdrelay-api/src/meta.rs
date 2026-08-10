//! Stable, public compatibility contract for independently deployed clients.

use axum::{Json, http::header::CACHE_CONTROL, response::IntoResponse};
use serde::Serialize;
use std::collections::BTreeMap;

const API_VERSION: &str = "1";
const SCHEMA_VERSION: u32 = 38;
const CACHE: &str = "public, max-age=30, s-maxage=30, stale-while-revalidate=60";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MetaResponse {
    api_version: &'static str,
    schema_version: u32,
    release: &'static str,
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
        ("ticketing_v1", true),
        ("staff_device_sessions_v2", true),
        ("viryaos_ops_v1", true),
    ]);
    (
        [(CACHE_CONTROL, CACHE)],
        Json(MetaResponse {
            api_version: API_VERSION,
            schema_version: SCHEMA_VERSION,
            release: option_env!("CROWDRELAY_RELEASE").unwrap_or(env!("CARGO_PKG_VERSION")),
            build_timestamp: option_env!("CROWDRELAY_BUILD_TIMESTAMP"),
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
        assert_eq!(SCHEMA_VERSION, 38);
    }
}
