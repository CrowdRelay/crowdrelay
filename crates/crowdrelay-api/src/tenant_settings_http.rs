//! Operator surface for per-tenant brand settings.
//!
//! GET returns the EFFECTIVE values merged over shipped defaults plus the list
//! of keys actually overridden, so a panel can show both without guessing.
//! PUT accepts only keys from `EDITABLE_KEYS`; every write lands in
//! `crowdrelay-infra::tenant_settings` (api-sql ratchet) and invalidates the
//! read cache there. The same handlers are re-exported under
//! `/v1/control-plane/tenant-settings*` for platform-plane forwarding.

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use crowdrelay_infra::tenant_settings::{EDITABLE_KEYS, TenantSettingsRepository};
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";

fn repository(state: &crate::AppState) -> TenantSettingsRepository {
    TenantSettingsRepository::new(state.database.clone())
}

#[derive(Serialize)]
pub struct BrandSettingsResponse {
    pub settings: HashMap<String, String>,
    pub overridden: Vec<String>,
    pub editable_keys: Vec<&'static str>,
}

pub async fn get_brand_settings(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let repository = repository(&state);
    match tokio::join!(
        repository.brand_settings(workspace_id),
        repository.list_overrides(workspace_id)
    ) {
        (Ok(effective), Ok(overrides)) => {
            let mut settings = HashMap::new();
            let effective: &crowdrelay_infra::tenant_settings::TenantBrandSettings =
                effective.as_ref();
            settings.insert(
                "member_site_base_url".to_owned(),
                effective.member_site_base_url.clone(),
            );
            settings.insert(
                "member_area_path".to_owned(),
                effective.member_area_path.clone(),
            );
            settings.insert(
                "synesthesia_campaign_slug".to_owned(),
                effective.synesthesia_campaign_slug.clone(),
            );
            (
                StatusCode::OK,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(BrandSettingsResponse {
                    overridden: overrides.into_keys().collect(),
                    editable_keys: EDITABLE_KEYS.to_vec(),
                    settings,
                }),
            )
                .into_response()
        }
        (Err(error), _) | (_, Err(error)) => {
            tracing::warn!(%error, "tenant settings lookup failed");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateSettingRequest {
    value: String,
}

fn validate_key(key: &str) -> bool {
    EDITABLE_KEYS.contains(&key)
}

fn validate_value(key: &str, value: &str) -> bool {
    if value.trim().is_empty() || value.len() > 512 {
        return false;
    }
    // The area path becomes a URL segment; keep it URL-safe like the defaults.
    if key == "member_area_path"
        && !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_'))
    {
        return false;
    }
    true
}

pub async fn upsert_setting(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(key): Path<String>,
    payload: Result<Json<UpdateSettingRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(request)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    if !validate_key(&key) || !validate_value(&key, &request.value) {
        return Problem::unprocessable(request_id_value).into_response();
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    match repository(&state)
        .set_setting(workspace_id, &key, request.value.trim())
        .await
    {
        Ok(()) => {
            let updated = repository(&state)
                .brand_settings(workspace_id)
                .await
                .map(|effective| {
                    let value = match key.as_str() {
                        "member_site_base_url" => effective.member_site_base_url.clone(),
                        "member_area_path" => effective.member_area_path.clone(),
                        _ => effective.synesthesia_campaign_slug.clone(),
                    };
                    serde_json::json!({ "key": key, "value": value })
                })
                .unwrap_or_else(|_| serde_json::json!({ "key": key }));
            (
                StatusCode::OK,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(updated),
            )
                .into_response()
        }
        Err(error) => {
            tracing::warn!(%error, "tenant setting update failed");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}
