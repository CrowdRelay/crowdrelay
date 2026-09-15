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

/// The north stars a tenant may choose, derived from the domain vocabulary.
///
/// Served rather than hard-coded in the operator UI because the list is a
/// domain fact: it is every platform that reports an audience size, plus the
/// two first-party metrics. The control plane used to keep its own copy of
/// four options, which silently stopped matching the day the vocabulary grew —
/// a tenant measured on SoundCloud could not pick SoundCloud because a
/// TypeScript literal had never heard of it.
///
/// `requiresSignal` lets the UI hide the Signal north star from a tenant that
/// has Signal switched off, without the UI needing to know why.
pub async fn list_north_star_options(headers: HeaderMap) -> Response {
    use crowdrelay_domain::growth_metrics::NorthStarMetric;

    let options: Vec<serde_json::Value> = NorthStarMetric::all()
        .into_iter()
        .map(|metric| {
            serde_json::json!({
                "value": metric.as_str(),
                "label": metric.display_name(),
                "requiresSignal": metric == NorthStarMetric::SignalInstalls,
                "isAggregate": metric.is_total_audience(),
                "platform": metric.platform().map(|platform| platform.as_str()),
            })
        })
        .collect();
    let _ = request_id(&headers);
    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(serde_json::json!({ "options": options })),
    )
        .into_response()
}

pub async fn get_brand_settings(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let repository = repository(&state);
    let budget = &state.read_budget;
    let joined = tokio::time::timeout(state.ticketing.operation_timeout(), async {
        tokio::join!(
            crate::ops::hold(budget, repository.brand_settings(workspace_id)),
            crate::ops::hold(budget, repository.list_overrides(workspace_id)),
            crate::ops::hold(budget, repository.cadence_settings(workspace_id))
        )
    })
    .await;
    let joined = match joined {
        Ok(results) => results,
        Err(_) => {
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };
    match joined {
        (Ok(effective), Ok(overrides), Ok(cadence)) => {
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
            settings.insert(
                "signal_enabled".to_owned(),
                if effective.signal_enabled {
                    "true"
                } else {
                    "false"
                }
                .to_owned(),
            );
            settings.insert(
                "synesthesia_enabled".to_owned(),
                if effective.synesthesia_enabled {
                    "true"
                } else {
                    "false"
                }
                .to_owned(),
            );
            settings.insert(
                "north_star_metric".to_owned(),
                effective.north_star_metric.clone(),
            );
            settings.insert(
                "social_auto_post".to_owned(),
                if effective.social_auto_post {
                    "true"
                } else {
                    "false"
                }
                .to_owned(),
            );
            settings.insert(
                "growth_cadence_moments_per_month".to_owned(),
                cadence.serious_moments_per_month.to_string(),
            );
            settings.insert(
                "growth_cadence_fillers_enabled".to_owned(),
                if cadence.fillers_enabled {
                    "true"
                } else {
                    "false"
                }
                .to_owned(),
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
        (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
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
    // Boolean keys accept only "true" or "false".
    if key == "signal_enabled"
        || key == "synesthesia_enabled"
        || key == "social_auto_post"
        || key == "growth_cadence_fillers_enabled"
    {
        return value == "true" || value == "false";
    }
    // North star metric must be a valid enum value.
    if key == "north_star_metric" {
        return crowdrelay_domain::growth_metrics::NorthStarMetric::parse(value).is_some();
    }
    // The cadence commitment is 1–4 serious moments a month — past weekly,
    // nothing is a serious moment any more.
    if key == "growth_cadence_moments_per_month" {
        return value
            .parse::<u8>()
            .ok()
            .is_some_and(|moments| (1..=4).contains(&moments));
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
            let repository = repository(&state);
            let updated = match key.as_str() {
                "growth_cadence_moments_per_month" | "growth_cadence_fillers_enabled" => repository
                    .cadence_settings(workspace_id)
                    .await
                    .map(|cadence| {
                        let value = if key == "growth_cadence_moments_per_month" {
                            cadence.serious_moments_per_month.to_string()
                        } else if cadence.fillers_enabled {
                            "true".to_owned()
                        } else {
                            "false".to_owned()
                        };
                        serde_json::json!({ "key": key, "value": value })
                    }),
                _ => repository
                    .brand_settings(workspace_id)
                    .await
                    .map(|effective| {
                        let value = match key.as_str() {
                            "member_site_base_url" => effective.member_site_base_url.clone(),
                            "member_area_path" => effective.member_area_path.clone(),
                            "signal_enabled" => if effective.signal_enabled {
                                "true"
                            } else {
                                "false"
                            }
                            .to_owned(),
                            "synesthesia_enabled" => if effective.synesthesia_enabled {
                                "true"
                            } else {
                                "false"
                            }
                            .to_owned(),
                            "north_star_metric" => effective.north_star_metric.clone(),
                            "social_auto_post" => if effective.social_auto_post {
                                "true"
                            } else {
                                "false"
                            }
                            .to_owned(),
                            _ => effective.synesthesia_campaign_slug.clone(),
                        };
                        serde_json::json!({ "key": key, "value": value })
                    }),
            }
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
