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
use crowdrelay_domain::gig_plan::TenantIntent;
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

/// The intents a band may state, from the planner's own vocabulary.
///
/// Served for the same reason the north stars are: the control plane used to
/// keep its own copy of a domain list, and it silently stopped matching the day
/// the list grew. An intent the console cannot offer is an intent the planner
/// will never respect, and the band would have no way to find that out.
pub async fn list_tenant_intent_options(headers: HeaderMap) -> Response {
    let options: Vec<serde_json::Value> = TenantIntent::all()
        .into_iter()
        .map(|intent| {
            serde_json::json!({
                "value": intent.as_str(),
                "description": intent.describe(),
                // The one choice that stops proposals entirely. The console
                // should say so at the moment of choosing, not afterwards.
                "withholdsProposals": intent == TenantIntent::HeadsDown,
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
            crate::ops::hold(budget, repository.cadence_settings(workspace_id)),
            crate::ops::hold(budget, repository.crew_locale(workspace_id))
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
        (Ok(effective), Ok(overrides), Ok(cadence), Ok(crew_locale)) => {
            let mut settings = HashMap::new();
            let effective: &crowdrelay_infra::tenant_settings::TenantBrandSettings =
                effective.as_ref();
            // The weekly ask ceiling (§4i-6) has a default, and the panel
            // shows it. An empty field used to mean uncapped, which is what
            // it was — a person's attention was the one quantity here with no
            // ceiling, and the one that ran out. The `overridden` list still
            // says whether the tenant chose this number or inherited it.
            settings.insert(
                "team_weekly_ask_ceiling".to_owned(),
                overrides
                    .get("team_weekly_ask_ceiling")
                    .cloned()
                    .unwrap_or_else(|| {
                        crowdrelay_domain::team_operations::DEFAULT_WEEKLY_ASK_CEILING.to_string()
                    }),
            );
            // §4G.2: absent means the band has never stated an intent, and the
            // effective value is `unstated` — which the planner acts on. The
            // `overridden` list still says whether they chose it or were never
            // asked, and those read differently to an operator.
            settings.insert(
                "tenant_intent".to_owned(),
                overrides
                    .get("tenant_intent")
                    .cloned()
                    .unwrap_or_else(|| TenantIntent::default().as_str().to_owned()),
            );
            // §5.21: absent is a real state — the act has not said what it
            // sounds like, and the pairing that needs it says so rather than
            // guessing. Empty string is how this surface spells absent, the
            // same way the weekly ask ceiling does.
            settings.insert(
                "act_style".to_owned(),
                overrides.get("act_style").cloned().unwrap_or_default(),
            );
            // Same absent-spelling as `act_style`: empty means the act has
            // not declared a home city and letters name none.
            settings.insert(
                "act_home_city".to_owned(),
                overrides.get("act_home_city").cloned().unwrap_or_default(),
            );
            // Empty means "use the workspace's own name" — the resolver's
            // ordinary path, not a missing value.
            settings.insert(
                "brand_wordmark".to_owned(),
                overrides.get("brand_wordmark").cloned().unwrap_or_default(),
            );
            settings.insert(
                "member_site_base_url".to_owned(),
                effective.member_site_base_url.clone(),
            );
            settings.insert(
                "member_area_path".to_owned(),
                effective.member_area_path.clone(),
            );
            settings.insert(
                "live_page_path".to_owned(),
                effective.live_page_path.clone(),
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
                "ticketing_enabled".to_owned(),
                if effective.ticketing_enabled {
                    "true"
                } else {
                    "false"
                }
                .to_owned(),
            );
            settings.insert("crew_locale".to_owned(), crew_locale.clone());
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
            // §5 join-ask: variants surface the stored JSON text verbatim —
            // absent means the feature is off and the field reads empty,
            // the same way `act_style` spells "never stated". Cadence and
            // platforms carry their shipped defaults instead, so the panel
            // shows the rule the evaluator will actually apply.
            settings.insert(
                "join_ask_variants".to_owned(),
                overrides
                    .get("join_ask_variants")
                    .cloned()
                    .unwrap_or_default(),
            );
            settings.insert(
                "join_ask_cadence_days".to_owned(),
                overrides
                    .get("join_ask_cadence_days")
                    .cloned()
                    .unwrap_or_else(|| {
                        crowdrelay_domain::join_ask::DEFAULT_JOIN_ASK_CADENCE_DAYS.to_string()
                    }),
            );
            settings.insert(
                "join_ask_platforms".to_owned(),
                overrides
                    .get("join_ask_platforms")
                    .cloned()
                    .unwrap_or_else(|| {
                        crowdrelay_domain::join_ask::DEFAULT_JOIN_ASK_PLATFORMS.join(",")
                    }),
            );
            // Empty is the unset spelling here too — the picker shows the
            // preview only when a real URL is stored.
            settings.insert(
                "join_ask_image_url".to_owned(),
                overrides
                    .get("join_ask_image_url")
                    .cloned()
                    .unwrap_or_default(),
            );
            // The autopost lanes surface their effective value, same as the
            // join-ask defaults: absent stores nothing and means "every
            // platform the executor can post".
            settings.insert(
                "social_autopost_platforms".to_owned(),
                overrides
                    .get("social_autopost_platforms")
                    .cloned()
                    .unwrap_or_else(|| {
                        crowdrelay_domain::social_autopost::DEFAULT_AUTOPOST_PLATFORMS.join(",")
                    }),
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
        (Err(error), _, _, _)
        | (_, Err(error), _, _)
        | (_, _, Err(error), _)
        | (_, _, _, Err(error)) => {
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
    // §5: a variants list is JSON text, and five 500-char variants plus
    // syntax do not fit under the generic 512 ceiling — it has its own
    // bounds, checked by the same domain contract the reader applies.
    if key == "join_ask_variants" {
        return value.len() <= 4_096
            && crowdrelay_domain::join_ask::parse_variants(value).is_some();
    }
    // The join-ask image is the one setting where empty is a statement:
    // clearing it removes the fixed image, which is a choice, not a missing
    // value. Present, it must be a fetchable https URL.
    if key == "join_ask_image_url" {
        return value.trim().is_empty()
            || crowdrelay_domain::join_ask::parse_image_url(value).is_some();
    }
    if value.trim().is_empty() || value.len() > 512 {
        return false;
    }
    if key == "join_ask_cadence_days" {
        return crowdrelay_domain::join_ask::parse_cadence_days(value).is_some();
    }
    if key == "join_ask_platforms" {
        return crowdrelay_domain::join_ask::parse_platforms(value).is_some();
    }
    if key == "social_autopost_platforms" {
        return crowdrelay_domain::social_autopost::parse_autopost_platforms(value).is_some();
    }
    // The area and live-page paths become URL segments; keep them URL-safe
    // like the defaults.
    if (key == "member_area_path" || key == "live_page_path")
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
        || key == "ticketing_enabled"
    {
        return value == "true" || value == "false";
    }
    // A language tag, not free text: this decides which wording a crew member
    // gets, and a typo would silently fall back to English forever.
    if key == "crew_locale" {
        let mut parts = value.split(['-', '_']);
        let language = parts.next().unwrap_or_default();
        let region = parts.next();
        return parts.next().is_none()
            && language.len() == 2
            && language.bytes().all(|b| b.is_ascii_lowercase())
            && region.is_none_or(|region| {
                region.len() == 2 && region.bytes().all(|b| b.is_ascii_alphabetic())
            });
    }
    // North star metric must be a valid enum value.
    if key == "north_star_metric" {
        return crowdrelay_domain::growth_metrics::NorthStarMetric::parse(value).is_some();
    }
    // §4h-8 / 5.21: the act's own words for what it sounds like. Free text,
    // because a controlled vocabulary would be a guess about scenes nobody
    // here belongs to — bounded so it stays a descriptor rather than a bio.
    if key == "act_style" {
        return value.chars().count() <= 120;
    }
    // A city name, not a bio — it lands mid-sentence in a letter's opening
    // line, so it is short, one line and free of control characters.
    if key == "act_home_city" {
        return value.trim().chars().count() <= 60 && !value.chars().any(char::is_control);
    }
    // It opens every push title an act's fans get ("{name} — new show"), so it
    // is a name, not a sentence: short, one line, no control characters that a
    // lock screen would render as boxes or break on.
    if key == "brand_wordmark" {
        return value.trim().chars().count() <= 40 && !value.chars().any(char::is_control);
    }
    // §4G.2: the gig planner reads this and refuses outright on `heads_down`.
    // A value it cannot parse would be stored and then ignored, which is the
    // worst of both — the band believes it said something and the planner never
    // heard it. Rejected at the edge instead.
    if key == "tenant_intent" {
        return crowdrelay_domain::gig_plan::TenantIntent::parse(value).is_some();
    }
    // The cadence commitment is 1–4 serious moments a month — past weekly,
    // nothing is a serious moment any more.
    if key == "growth_cadence_moments_per_month" {
        return value
            .parse::<u8>()
            .ok()
            .is_some_and(|moments| (1..=4).contains(&moments));
    }
    // §4i-6: the weekly ask ceiling is a small integer; 0 would silently
    // disable every handoff, so the floor is 1 and the ceiling generous.
    if key == "team_weekly_ask_ceiling" {
        return value
            .parse::<u16>()
            .ok()
            .is_some_and(|asks| (1..=500).contains(&asks));
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
            // The sender line of every letter still waiting on a person is
            // re-composed from what the band just said about itself — a fixed
            // home city must not wait for each stale draft to expire.
            if matches!(
                key.as_str(),
                "act_home_city" | "act_style" | "member_site_base_url"
            ) {
                match state
                    .autopilot
                    .recompose_pending_letters(state.ticketing.workspace_id())
                    .await
                {
                    Ok(changed) if changed > 0 => {
                        tracing::info!(changed, key = %key, "pending letters re-composed");
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(error = %error, "pending letters could not be re-composed");
                    }
                }
            }
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
                            "live_page_path" => effective.live_page_path.clone(),
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
                            "ticketing_enabled" => if effective.ticketing_enabled {
                                "true"
                            } else {
                                "false"
                            }
                            .to_owned(),
                            "synesthesia_campaign_slug" => {
                                effective.synesthesia_campaign_slug.clone()
                            }
                            // Keys `TenantBrandSettings` does not carry —
                            // `crew_locale`, `team_weekly_ask_ceiling`,
                            // `tenant_intent` — echo the value that was just
                            // accepted. The previous fallback returned the
                            // synesthesia campaign slug for all three, so a
                            // console saving a crew locale was shown a campaign
                            // slug as the new value.
                            _ => request.value.trim().to_owned(),
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

#[cfg(test)]
mod tests {
    use super::{validate_key, validate_value};

    #[test]
    fn join_ask_keys_are_editable() {
        for key in [
            "join_ask_variants",
            "join_ask_cadence_days",
            "join_ask_platforms",
        ] {
            assert!(validate_key(key), "{key} must be an editable key");
        }
    }

    #[test]
    fn join_ask_variants_accept_the_contract_shape() {
        assert!(validate_value(
            "join_ask_variants",
            r#"["Join us on Signal","Come along"]"#
        ));
        // Rejected shapes — each fails for its own reason, so a validator
        // that accepted everything would fail these.
        assert!(!validate_value("join_ask_variants", "not json"));
        assert!(!validate_value("join_ask_variants", r#"{"a":1}"#));
        assert!(!validate_value("join_ask_variants", "[]"));
        // Six variants exceeds the 1–5 bound.
        assert!(!validate_value(
            "join_ask_variants",
            r#"["a","b","c","d","e","f"]"#
        ));
        // A variant over 500 characters (after trim) is a blog post, not an ask.
        let too_long = format!(r#"["{}"]"#, "x".repeat(501));
        assert!(!validate_value("join_ask_variants", &too_long));
        // …and an empty string is not a variant.
        assert!(!validate_value("join_ask_variants", r#"[""]"#));
    }

    #[test]
    fn join_ask_cadence_stays_inside_its_bounds() {
        assert!(validate_value("join_ask_cadence_days", "7"));
        assert!(validate_value("join_ask_cadence_days", "3"));
        assert!(validate_value("join_ask_cadence_days", "30"));
        assert!(!validate_value("join_ask_cadence_days", "2"));
        assert!(!validate_value("join_ask_cadence_days", "31"));
        assert!(!validate_value("join_ask_cadence_days", "weekly"));
    }

    #[test]
    fn join_ask_platforms_reject_unknown_channels() {
        assert!(validate_value("join_ask_platforms", "facebook,instagram"));
        assert!(validate_value(
            "join_ask_platforms",
            "facebook,instagram,telegram,discord"
        ));
        assert!(!validate_value("join_ask_platforms", "facebook,tiktok"));
        assert!(!validate_value("join_ask_platforms", ""));
    }

    #[test]
    fn a_wordmark_is_a_short_single_line_name() {
        assert!(validate_key("brand_wordmark"));
        assert!(validate_value("brand_wordmark", "VIRYA"));
        assert!(validate_value("brand_wordmark", "MGŁA"));
        assert!(!validate_value("brand_wordmark", "The Band\nNew show"));
        assert!(!validate_value("brand_wordmark", &"x".repeat(41)));
        assert!(!validate_value("brand_wordmark", "   "));
    }
}
