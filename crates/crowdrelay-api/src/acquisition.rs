//! HTTP transport for public acquisition endpoints.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::sync::{Mutex, RwLock};

use axum::{
    Json,
    body::{Body, Bytes},
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{
            CACHE_CONTROL, CONTENT_TYPE, COOKIE, ETAG, IF_NONE_MATCH, LOCATION, REFERER,
            REFERRER_POLICY, SET_COOKIE,
        },
    },
    response::{IntoResponse, Response},
};
use crowdrelay_application::{
    AcquisitionRepository, IdempotencyKey, ListCities, ListCitiesError, RedirectCache,
    RepositoryError, RequestId, SignupFan, SignupFanCommand, SignupFanError,
    UpsertSmartLinkCommand,
};
use crowdrelay_domain::{
    CampaignId, CitySlug, ClickEvent, FanId, FanSessionToken, FanSignup, FanSignupEmailKind,
    FanSignupInput, FanStatus, MarketingConsent, NormalizedEmail, ReferralCode, SmartLinkSlug,
    VisitorId, WorkspaceId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use url::Url;
use uuid::Uuid;

use crate::{IDEMPOTENCY_KEY, Problem, X_REQUEST_ID, request_id};

const ATTRIBUTION_COOKIE: &str = "crowdrelay_attribution";
const REFERRAL_COOKIE: &str = "crowdrelay_referral";
const FAN_SESSION_COOKIE: &str = "crowdrelay_fan";
const ATTRIBUTION_COOKIE_MAX_AGE_SECONDS: u32 = 30 * 24 * 60 * 60;
const FAN_SESSION_COOKIE_MAX_AGE_SECONDS: u32 = 90 * 24 * 60 * 60;
const PRIVATE_NO_STORE: &str = "private, no-store";
const PUBLIC_CITY_CACHE: &str =
    "public, max-age=60, stale-while-revalidate=600, stale-if-error=86400";
const CITY_SNAPSHOT_MAX_AGE: Duration = Duration::from_secs(5 * 60);
const MAX_CITY_SNAPSHOT_LIMIT: u32 = 100;
const DEFAULT_CITY_LIMIT: u32 = 20;

/// Closure that accepts a click event for asynchronous batched persistence.
pub type ClickSubmitter = Arc<dyn Fn(ClickEvent) + Send + Sync>;
/// Closure that returns a point-in-time snapshot of click ingestion counters.
pub type ClickMetricsReader = Arc<dyn Fn() -> ClickMetricsSnapshot + Send + Sync>;

/// Point-in-time counters for the click ingestion pipeline.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ClickMetricsSnapshot {
    /// Total click events accepted by the bounded buffer.
    pub queued: u64,
    /// Total click events durably written to PostgreSQL.
    pub persisted: u64,
    /// Total click events dropped under overload or shutdown.
    pub dropped: u64,
    /// Total click events lost after a bounded persistence failure.
    pub persistence_failed: u64,
}

#[derive(Debug, Default)]
struct CitySnapshot {
    items: Arc<Vec<crowdrelay_domain::CitySignal>>,
    refreshed_at: Option<Instant>,
    /// Serialized responses for this exact `items` snapshot, keyed by limit.
    rendered: HashMap<u32, Arc<RenderedCities>>,
}

/// Construction parameters for acquisition HTTP state.
pub struct AcquisitionStateArgs {
    pub workspace_id: WorkspaceId,
    pub redirect_cache: Arc<RedirectCache>,
    pub signup_fan: SignupFan,
    pub list_cities: ListCities,
    pub click_submitter: ClickSubmitter,
    pub click_metrics_reader: ClickMetricsReader,
    pub public_site_base_url: Url,
    pub secure_cookies: bool,
    pub acquisition_repository: Arc<dyn AcquisitionRepository>,
}

/// Dependencies and trusted tenant context used by public acquisition routes.
#[derive(Clone)]
pub struct AcquisitionState {
    workspace_id: WorkspaceId,
    redirect_cache: Arc<RedirectCache>,
    signup_fan: SignupFan,
    list_cities: ListCities,
    city_snapshot: Arc<RwLock<CitySnapshot>>,
    city_refresh: Arc<Mutex<()>>,
    click_submitter: ClickSubmitter,
    click_metrics_reader: ClickMetricsReader,
    public_site_base_url: Url,
    secure_cookies: bool,
    acquisition_repository: Arc<dyn AcquisitionRepository>,
}

impl AcquisitionState {
    /// Creates acquisition route state for one trusted workspace.
    #[must_use]
    pub fn new(args: AcquisitionStateArgs) -> Self {
        Self {
            workspace_id: args.workspace_id,
            redirect_cache: args.redirect_cache,
            signup_fan: args.signup_fan,
            list_cities: args.list_cities,
            city_snapshot: Arc::new(RwLock::new(CitySnapshot::default())),
            city_refresh: Arc::new(Mutex::new(())),
            click_submitter: args.click_submitter,
            click_metrics_reader: args.click_metrics_reader,
            public_site_base_url: args.public_site_base_url,
            secure_cookies: args.secure_cookies,
            acquisition_repository: args.acquisition_repository,
        }
    }

    pub(crate) fn click_metrics_snapshot(&self) -> ClickMetricsSnapshot {
        (self.click_metrics_reader)()
    }

    pub(crate) fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    pub(crate) fn acquisition_repository(&self) -> &Arc<dyn AcquisitionRepository> {
        &self.acquisition_repository
    }

    async fn cached_snapshot(
        &self,
        max_age: Option<Duration>,
    ) -> Option<Arc<Vec<crowdrelay_domain::CitySignal>>> {
        let snapshot = self.city_snapshot.read().await;
        let refreshed_at = snapshot.refreshed_at?;
        if max_age.is_some_and(|age| refreshed_at.elapsed() > age) {
            return None;
        }
        Some(Arc::clone(&snapshot.items))
    }

    /// Resolves a public city slug to its id.
    ///
    /// Operator surfaces render the public city list, which carries slugs and no
    /// ids, so writes that key on a city id need this translation. It reads the
    /// same cached snapshot the public endpoint serves rather than adding an
    /// admin city read.
    pub(crate) async fn city_id_for_slug(
        &self,
        slug: &crowdrelay_domain::CitySlug,
    ) -> Result<Option<crowdrelay_domain::CityId>, ListCitiesError> {
        Ok(self
            .resilient_snapshot()
            .await?
            .iter()
            .find(|city| city.slug() == slug)
            .map(crowdrelay_domain::CitySignal::city_id))
    }

    /// Returns the shared city snapshot, refreshing it at most once per
    /// `CITY_SNAPSHOT_MAX_AGE` and falling back to the previous snapshot when a
    /// refresh fails.
    async fn resilient_snapshot(
        &self,
    ) -> Result<Arc<Vec<crowdrelay_domain::CitySignal>>, ListCitiesError> {
        if let Some(cities) = self.cached_snapshot(Some(CITY_SNAPSHOT_MAX_AGE)).await {
            return Ok(cities);
        }

        let _refresh = self.city_refresh.lock().await;
        if let Some(cities) = self.cached_snapshot(Some(CITY_SNAPSHOT_MAX_AGE)).await {
            return Ok(cities);
        }
        let stale = self.cached_snapshot(None).await;
        match self
            .list_cities
            .execute(self.workspace_id, MAX_CITY_SNAPSHOT_LIMIT)
            .await
        {
            Ok(cities) => {
                let items = Arc::new(cities);
                let mut snapshot = self.city_snapshot.write().await;
                snapshot.items = Arc::clone(&items);
                snapshot.refreshed_at = Some(Instant::now());
                // Rendered responses describe the replaced snapshot only.
                snapshot.rendered.clear();
                Ok(items)
            }
            Err(error) => match stale {
                Some(cities) => {
                    tracing::warn!(%error, "city refresh failed; serving previous snapshot");
                    Ok(cities)
                }
                None => Err(error),
            },
        }
    }

    /// Returns the serialized public city list for `limit`, rendering and
    /// caching it once per snapshot instead of per request.
    async fn rendered_cities(&self, limit: u32) -> Result<Arc<RenderedCities>, RenderCitiesError> {
        if !(1..=MAX_CITY_SNAPSHOT_LIMIT).contains(&limit) {
            return Err(RenderCitiesError::List(ListCitiesError::InvalidLimit {
                max: MAX_CITY_SNAPSHOT_LIMIT,
            }));
        }
        let items = self
            .resilient_snapshot()
            .await
            .map_err(RenderCitiesError::List)?;
        {
            let snapshot = self.city_snapshot.read().await;
            if Arc::ptr_eq(&snapshot.items, &items)
                && let Some(rendered) = snapshot.rendered.get(&limit)
            {
                return Ok(Arc::clone(rendered));
            }
        }

        let body = serde_json::to_vec(&CityListResponse {
            items: items
                .iter()
                .take(usize::try_from(limit).unwrap_or(usize::MAX))
                .cloned()
                .map(CitySignalResponse::from)
                .collect(),
        })
        .map_err(|_| RenderCitiesError::Serialization)?;
        let etag = format!("\"cities-{}\"", hex::encode(Sha256::digest(&body)));
        let etag_header =
            HeaderValue::from_str(&etag).map_err(|_| RenderCitiesError::Serialization)?;
        let rendered = Arc::new(RenderedCities {
            body: Bytes::from(body),
            etag,
            etag_header,
        });

        let mut snapshot = self.city_snapshot.write().await;
        // A refresh may have replaced the snapshot while this response was
        // rendered; that render then describes a superseded snapshot and must
        // not be cached against the current one.
        if Arc::ptr_eq(&snapshot.items, &items) {
            snapshot.rendered.insert(limit, Arc::clone(&rendered));
        }
        Ok(rendered)
    }
}

/// A serialized public city list and its validator, cached per snapshot.
#[derive(Debug)]
struct RenderedCities {
    body: Bytes,
    etag: String,
    etag_header: HeaderValue,
}

enum RenderCitiesError {
    List(ListCitiesError),
    Serialization,
}

/// Resolves a smart link, records non-critical attribution, and redirects immediately.
pub async fn redirect_smart_link(
    State(state): State<crate::AppState>,
    Path(raw_slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Ok(slug) = SmartLinkSlug::parse(&raw_slug) else {
        return Problem::not_found(request_id(&headers))
            .private()
            .into_response();
    };
    // The snapshot Arc is held for the whole handler, so the redirect reads one
    // consistent view without copying the smart-link on every request.
    let snapshot = state.acquisition.redirect_cache.snapshot();
    let Some(link) = snapshot.resolve(state.acquisition.workspace_id, &slug) else {
        return Problem::not_found(request_id(&headers))
            .private()
            .into_response();
    };

    let visitor_id = attribution_visitor(&headers).unwrap_or_default();
    let referrer_host = referrer_host(&headers);
    match ClickEvent::from_link(
        link,
        Some(visitor_id),
        referrer_host,
        OffsetDateTime::now_utc(),
    ) {
        Ok(event) => (state.acquisition.click_submitter)(event),
        Err(error) => {
            // Analytics is explicitly best effort. A malformed referrer must
            // never delay or break the redirect path.
            tracing::debug!(%error, "discarded invalid click referrer metadata");
        }
    }

    let Ok(location) = HeaderValue::from_str(link.destination_url().as_str()) else {
        tracing::error!(
            smart_link_id = %link.id(),
            "validated smart-link destination could not be encoded as a response header"
        );
        return Problem::internal(request_id(&headers))
            .private()
            .into_response();
    };
    let Ok(cookie) = HeaderValue::from_str(&attribution_cookie(
        visitor_id,
        state.acquisition.secure_cookies,
    )) else {
        tracing::error!("attribution cookie could not be encoded as a response header");
        return Problem::internal(request_id(&headers))
            .private()
            .into_response();
    };

    (
        StatusCode::FOUND,
        [
            (LOCATION, location),
            (SET_COOKIE, cookie),
            (CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE)),
            (REFERRER_POLICY, HeaderValue::from_static("no-referrer")),
        ],
    )
        .into_response()
}

/// JSON body accepted by the fan signup endpoint.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FanSignupRequest {
    email: String,
    display_name: Option<String>,
    city_slug: String,
    locale: Option<String>,
    referral_code: Option<String>,
    campaign_id: Option<CampaignId>,
    consent: ConsentRequest,
    #[serde(default)]
    nearby_gigs: NearbyGigsRequest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NearbyGigsRequest {
    #[serde(default = "default_nearby_enabled")]
    enabled: bool,
    #[serde(default = "default_nearby_radius")]
    radius_km: i32,
}

impl Default for NearbyGigsRequest {
    fn default() -> Self {
        Self {
            enabled: true,
            radius_km: 150,
        }
    }
}

const fn default_nearby_enabled() -> bool {
    true
}

const fn default_nearby_radius() -> i32 {
    150
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConsentRequest {
    marketing: bool,
    policy_version: String,
}

#[derive(Serialize)]
struct FanSignupResponse {
    fan_id: crowdrelay_domain::FanId,
    status: FanStatus,
    referral_url: Option<String>,
    confirmation_required: bool,
    email_kind: Option<FanSignupEmailKind>,
    email_queued: bool,
    retry_after_seconds: Option<u32>,
}

/// Creates or updates a consented fan signup using an idempotent durable write.
pub async fn signup_fan(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<FanSignupRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(rejection) => {
            tracing::debug!(
                rejection = %rejection.status(),
                "rejected malformed fan signup JSON"
            );
            let problem = if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
                Problem::payload_too_large(request_id_value)
            } else {
                Problem::bad_request(request_id_value)
            };
            return problem.private().into_response();
        }
    };

    let idempotency_key = match headers
        .get(&IDEMPOTENCY_KEY)
        .and_then(|value| value.to_str().ok())
        .map(IdempotencyKey::parse)
    {
        Some(Ok(key)) => key,
        _ => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    let Some(raw_request_id) = headers
        .get(&X_REQUEST_ID)
        .and_then(|value| value.to_str().ok())
    else {
        tracing::error!("server request ID middleware did not populate the request");
        return Problem::internal(None).private().into_response();
    };
    let Ok(command_request_id) = RequestId::parse(raw_request_id) else {
        tracing::error!("server request ID did not pass application validation");
        return Problem::internal(None).private().into_response();
    };

    let nearby_enabled = payload.nearby_gigs.enabled;
    let nearby_radius_km = payload.nearby_gigs.radius_km;
    let requested_city_slug = payload.city_slug.clone();
    if !(25..=500).contains(&nearby_radius_km) {
        return Problem::unprocessable(request_id_value)
            .private()
            .into_response();
    }
    let signup = match build_signup(
        state.acquisition.workspace_id,
        attribution_visitor(&headers),
        referral_cookie(&headers),
        payload,
    ) {
        Ok(signup) => signup,
        Err(_) => {
            return Problem::unprocessable(request_id_value)
                .private()
                .into_response();
        }
    };
    let command = SignupFanCommand::new(idempotency_key, command_request_id, signup);

    let result = match state.acquisition.signup_fan.execute(&command).await {
        Ok(result) => result,
        Err(error) => return signup_error(error, request_id_value).into_response(),
    };
    let mobile_fan = crowdrelay_infra::mobile_fan::PostgresMobileFanRepository::new(
        state.ticketing.pool().clone(),
        state.acquisition.workspace_id,
        state.ticketing.operation_timeout(),
    );
    if let Err(error) = mobile_fan
        .upsert_fan_location_preference(
            result.fan_id,
            &requested_city_slug,
            nearby_enabled,
            nearby_radius_km,
        )
        .await
    {
        tracing::warn!(
            %error,
            fan_id = %result.fan_id,
            "fan signup completed but nearby preference could not be persisted"
        );
    }

    let status = if result.confirmation_required {
        StatusCode::ACCEPTED
    } else if result.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    let referral_url = match result.referral_code.as_ref() {
        Some(code) => match referral_url(&state.acquisition.public_site_base_url, code) {
            Ok(url) => Some(url),
            Err(_) => {
                tracing::error!("configured public site URL could not form a referral URL");
                return Problem::internal(request_id_value)
                    .private()
                    .into_response();
            }
        },
        None => None,
    };

    let mut response = (
        status,
        [(CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE))],
        Json(FanSignupResponse {
            fan_id: result.fan_id,
            status: result.status,
            referral_url,
            confirmation_required: result.confirmation_required,
            email_kind: result.email_kind,
            email_queued: result.email_queued,
            retry_after_seconds: result.retry_after_seconds,
        }),
    )
        .into_response();

    if let Some(token) = result.fan_session_token.as_ref() {
        let Ok(fan_cookie) =
            HeaderValue::from_str(&fan_session_cookie(token, state.acquisition.secure_cookies))
        else {
            tracing::error!("fan session cookie could not be encoded as a response header");
            return Problem::internal(request_id_value)
                .private()
                .into_response();
        };
        response.headers_mut().append(SET_COOKIE, fan_cookie);
    }
    response
}

fn build_signup(
    workspace_id: WorkspaceId,
    visitor_id: Option<VisitorId>,
    cookie_referral_code: Option<ReferralCode>,
    payload: FanSignupRequest,
) -> Result<FanSignup, SignupPayloadError> {
    let email = NormalizedEmail::parse(payload.email).map_err(|_| SignupPayloadError::Email)?;
    let city_slug = CitySlug::parse(payload.city_slug).map_err(|_| SignupPayloadError::City)?;
    let claimed_referral_code = payload
        .referral_code
        .map(ReferralCode::parse)
        .transpose()
        .map_err(|_| SignupPayloadError::ReferralCode)?
        .or(cookie_referral_code);
    let consent = MarketingConsent::new(
        payload.consent.marketing,
        payload.consent.policy_version,
        "public_signup",
    )
    .map_err(|_| SignupPayloadError::Consent)?;

    FanSignup::new(FanSignupInput {
        workspace_id,
        email,
        display_name: payload.display_name,
        city_slug,
        locale: payload.locale,
        campaign_id: payload.campaign_id,
        visitor_id,
        claimed_referral_code,
        consent,
    })
    .map_err(|_| SignupPayloadError::Signup)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SignupPayloadError {
    Email,
    City,
    ReferralCode,
    Consent,
    Signup,
}

fn signup_error(error: SignupFanError, request_id: Option<String>) -> Problem {
    let problem = match error {
        SignupFanError::InvalidInput(_) => Problem::unprocessable(request_id),
        SignupFanError::Repository(error) => repository_problem(error, request_id),
    };
    problem.private()
}

/// Optional query parameters for the public city listing endpoint.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CityQuery {
    limit: Option<u32>,
}

#[derive(Serialize)]
struct CityListResponse {
    items: Vec<CitySignalResponse>,
}

#[derive(Serialize)]
struct CitySignalResponse {
    slug: String,
    name: String,
    country_code: String,
    fan_count: u64,
}

impl From<crowdrelay_domain::CitySignal> for CitySignalResponse {
    fn from(signal: crowdrelay_domain::CitySignal) -> Self {
        Self {
            slug: signal.slug().as_str().to_owned(),
            name: signal.name().to_owned(),
            country_code: signal.country_code().as_str().to_owned(),
            fan_count: signal.fan_count(),
        }
    }
}

/// Returns the cacheable public city-demand leaderboard.
pub async fn list_cities(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    query: Result<Query<CityQuery>, QueryRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return Problem::bad_request(request_id_value).into_response(),
    };

    let rendered = match state
        .acquisition
        .rendered_cities(query.limit.unwrap_or(DEFAULT_CITY_LIMIT))
        .await
    {
        Ok(rendered) => rendered,
        Err(RenderCitiesError::List(ListCitiesError::InvalidLimit { .. })) => {
            return Problem::bad_request(request_id_value).into_response();
        }
        Err(RenderCitiesError::List(ListCitiesError::Repository(error))) => {
            return repository_problem(error, request_id_value).into_response();
        }
        Err(RenderCitiesError::Serialization) => {
            tracing::error!("failed to render the public city response");
            return Problem::internal(request_id_value).into_response();
        }
    };

    if etag_matches(headers.get(IF_NONE_MATCH), &rendered.etag) {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (ETAG, rendered.etag_header.clone()),
                (CACHE_CONTROL, HeaderValue::from_static(PUBLIC_CITY_CACHE)),
            ],
        )
            .into_response();
    }

    (
        StatusCode::OK,
        [
            (CONTENT_TYPE, HeaderValue::from_static("application/json")),
            (ETAG, rendered.etag_header.clone()),
            (CACHE_CONTROL, HeaderValue::from_static(PUBLIC_CITY_CACHE)),
        ],
        Body::from(rendered.body.clone()),
    )
        .into_response()
}

fn repository_problem(error: RepositoryError, request_id: Option<String>) -> Problem {
    match error {
        RepositoryError::Unavailable => {
            tracing::warn!("acquisition repository is temporarily unavailable");
        }
        RepositoryError::Unexpected => {
            tracing::error!("acquisition repository failed unexpectedly");
        }
        RepositoryError::NotFound | RepositoryError::Conflict => {}
    }

    match error {
        RepositoryError::Unavailable => Problem::service_unavailable(request_id),
        RepositoryError::NotFound => Problem::not_found(request_id),
        RepositoryError::Conflict => Problem::conflict(request_id),
        RepositoryError::Unexpected => Problem::internal(request_id),
    }
}

/// Reads the first-party anonymous visitor identifier from request cookies.
pub(crate) fn attribution_visitor(headers: &HeaderMap) -> Option<VisitorId> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .flat_map(|header| header.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find_map(|(name, value)| {
            (name == ATTRIBUTION_COOKIE)
                .then(|| value.parse::<VisitorId>().ok())
                .flatten()
        })
}

fn referral_cookie(headers: &HeaderMap) -> Option<ReferralCode> {
    cookie_value(headers, REFERRAL_COOKIE).and_then(|value| ReferralCode::parse(value).ok())
}

/// Reads the private fan-session token from request cookies.
pub(crate) fn fan_session_from_headers(headers: &HeaderMap) -> Option<FanSessionToken> {
    cookie_value(headers, FAN_SESSION_COOKIE).and_then(|value| FanSessionToken::parse(value).ok())
}

fn cookie_value<'a>(headers: &'a HeaderMap, expected_name: &str) -> Option<&'a str> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .flat_map(|header| header.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find_map(|(name, value)| (name == expected_name).then_some(value))
}

fn fan_session_cookie(token: &FanSessionToken, secure: bool) -> String {
    let secure_attribute = if secure { "; Secure" } else { "" };
    format!(
        "{FAN_SESSION_COOKIE}={}; Max-Age={FAN_SESSION_COOKIE_MAX_AGE_SECONDS}; \
         Path=/; HttpOnly; SameSite=Lax{secure_attribute}",
        token.as_str()
    )
}

fn attribution_cookie(visitor_id: VisitorId, secure: bool) -> String {
    let secure_attribute = if secure { "; Secure" } else { "" };
    format!(
        "{ATTRIBUTION_COOKIE}={visitor_id}; Max-Age={ATTRIBUTION_COOKIE_MAX_AGE_SECONDS}; \
         Path=/; HttpOnly; SameSite=Lax{secure_attribute}"
    )
}

/// Extracts a normalized referrer host for privacy-preserving attribution.
pub(crate) fn referrer_host(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(REFERER)?.to_str().ok()?;
    Url::parse(raw).ok()?.host_str().map(|host| host.to_owned())
}

fn referral_url(base_url: &Url, code: &ReferralCode) -> Result<String, url::ParseError> {
    base_url
        .join(&format!("r/{}", code.as_str()))
        .map(String::from)
}

fn etag_matches(candidate: Option<&HeaderValue>, expected: &str) -> bool {
    candidate
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.split(',').map(str::trim).any(|candidate| {
                candidate == "*"
                    || candidate == expected
                    || candidate.strip_prefix("W/") == Some(expected)
            })
        })
}

// ---------------------------------------------------------------------------
// Admin endpoints — campaign preparation, not autonomous work.
//
// The plan's first prerequisite for the acquisition campaign is "make every
// launch channel a tracked link". Smart links exist and the redirect path
// works, but there is no admin endpoint to create them — only the autopilot
// creates them internally, for releases and show-growth surfaces. An operator
// preparing a campaign across Reddit, Facebook, Bandsintown and Spotify needs
// to create one link per channel without waiting for the agent to invent a
// reason. The third prerequisite is "give the nineteen a referral code each":
// referral codes are generated on signup, but the nineteen signed up before
// the referral ledger existed, and there is no endpoint to backfill them.
//
// All SQL writes go through `AcquisitionRepository` — the API layer holds no
// sqlx call sites, per the architecture ratchet.
// ---------------------------------------------------------------------------

/// Request body for `POST /v1/admin/smart-links`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSmartLinkRequest {
    slug: String,
    destination_url: String,
    /// Broad channel: 'reddit', 'facebook', 'discord', 'linkedin', etc.
    /// Free text — inventing a channel is a Tuesday afternoon decision, and a
    /// constraint here would mean a migration every time somebody tries a new
    /// place (migration 0079).
    channel_source: Option<String>,
    /// The specific place inside that channel: which subreddit, which group.
    channel_community: Option<String>,
    /// Which post, image or wording. Two links to the same community with
    /// different creatives is how a test gets run without a testing framework.
    channel_creative: Option<String>,
    /// Optional campaign to associate the link with.
    campaign_id: Option<CampaignId>,
}

#[derive(Serialize)]
struct CreateSmartLinkResponse {
    id: Uuid,
    slug: String,
    destination_url: String,
    channel_source: Option<String>,
    channel_community: Option<String>,
    channel_creative: Option<String>,
    campaign_id: Option<CampaignId>,
    active: bool,
}

/// Creates a tracked smart link for a campaign channel. Idempotent on slug:
/// re-posting the same slug updates the destination and channel fields rather
/// than failing, so an operator can correct a URL without a delete + recreate.
pub async fn admin_create_smart_link(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreateSmartLinkRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return Problem::bad_request(request_id_value).into_response(),
    };

    let slug = match SmartLinkSlug::parse(&payload.slug) {
        Ok(value) => value,
        Err(_) => return Problem::bad_request(request_id_value).into_response(),
    };

    let destination = payload.destination_url.trim();
    if !destination.starts_with("https://") {
        return Problem::bad_request(request_id_value).into_response();
    }

    let channel_source = payload
        .channel_source
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let channel_community = payload
        .channel_community
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let channel_creative = payload
        .channel_creative
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let workspace_id = state.acquisition.workspace_id();
    let command = UpsertSmartLinkCommand {
        workspace_id,
        slug: &slug,
        destination_url: destination,
        channel_source,
        channel_community,
        channel_creative,
        campaign_id: payload.campaign_id,
    };
    let result = state
        .acquisition
        .acquisition_repository()
        .upsert_smart_link(&command)
        .await;

    let link = match result {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "admin smart-link creation failed");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };

    (
        StatusCode::CREATED,
        [(CACHE_CONTROL, HeaderValue::from_static("private, no-store"))],
        Json(CreateSmartLinkResponse {
            id: link.id,
            slug: link.slug.into_inner(),
            destination_url: link.destination_url,
            channel_source: link.channel_source,
            channel_community: link.channel_community,
            channel_creative: link.channel_creative,
            campaign_id: link.campaign_id,
            active: link.active,
        }),
    )
        .into_response()
}

/// Lists all smart links for the workspace, with channel attribution. The
/// operator's campaign preparation view: which links exist, which channels
/// they serve, and where the gaps are.
pub async fn admin_list_smart_links(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let workspace_id = state.acquisition.workspace_id();
    let result = state
        .acquisition
        .acquisition_repository()
        .list_smart_links(workspace_id)
        .await;

    let items = match result {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "admin smart-link listing failed");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };

    let items: Vec<SmartLinkView> = items
        .into_iter()
        .map(|link| SmartLinkView {
            id: link.id,
            slug: link.slug.into_inner(),
            destination_url: link.destination_url,
            active: link.active,
            channel_source: link.channel_source,
            channel_community: link.channel_community,
            channel_creative: link.channel_creative,
            campaign_id: link.campaign_id,
        })
        .collect();

    (
        StatusCode::OK,
        [(CACHE_CONTROL, HeaderValue::from_static("private, no-store"))],
        Json(SmartLinkListResponse { items }),
    )
        .into_response()
}

#[derive(Serialize)]
struct SmartLinkView {
    id: Uuid,
    slug: String,
    destination_url: String,
    active: bool,
    channel_source: Option<String>,
    channel_community: Option<String>,
    channel_creative: Option<String>,
    campaign_id: Option<CampaignId>,
}

#[derive(Serialize)]
struct SmartLinkListResponse {
    items: Vec<SmartLinkView>,
}

/// Creates a referral code for a fan who does not have one yet. Idempotent:
/// if the fan already has an active code, it is returned unchanged. The
/// nineteen fans who signed up before the referral ledger existed are the
/// reason this endpoint exists.
pub async fn admin_create_fan_referral_code(
    State(state): State<crate::AppState>,
    Path(fan_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let workspace_id = state.acquisition.workspace_id();
    let result = state
        .acquisition
        .acquisition_repository()
        .load_or_create_fan_referral_code(workspace_id, FanId::from_uuid(fan_id))
        .await;

    let code = match result {
        Ok(value) => value,
        Err(RepositoryError::NotFound) => {
            return Problem::not_found(request_id_value)
                .private()
                .into_response();
        }
        Err(error) => {
            tracing::warn!(%error, %fan_id, "admin referral-code creation failed");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };

    (
        StatusCode::CREATED,
        [(CACHE_CONTROL, HeaderValue::from_static("private, no-store"))],
        Json(ReferralCodeResponse {
            fan_id,
            code: code.into_inner(),
        }),
    )
        .into_response()
}

#[derive(Serialize)]
struct ReferralCodeResponse {
    fan_id: Uuid,
    code: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribution_cookie_has_required_security_attributes() {
        let visitor = VisitorId::new();
        let production = attribution_cookie(visitor, true);

        assert!(production.contains("HttpOnly"));
        assert!(production.contains("SameSite=Lax"));
        assert!(production.contains("Secure"));
        assert!(production.contains("Max-Age=2592000"));
        assert!(!production.contains("Domain="));

        assert!(!attribution_cookie(visitor, false).contains("; Secure"));
    }

    #[test]
    fn parses_only_the_named_valid_attribution_cookie() -> Result<(), Box<dyn std::error::Error>> {
        let visitor = VisitorId::new();
        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            HeaderValue::from_str(&format!("unrelated=value; {ATTRIBUTION_COOKIE}={visitor}"))?,
        );

        assert_eq!(attribution_visitor(&headers), Some(visitor));

        headers.insert(
            COOKIE,
            HeaderValue::from_static("crowdrelay_attribution=not-a-uuid"),
        );
        assert_eq!(attribution_visitor(&headers), None);
        Ok(())
    }

    #[test]
    fn etag_matching_accepts_lists_and_weak_validators() -> Result<(), Box<dyn std::error::Error>> {
        let expected = "\"cities-deadbeef\"";
        for raw in [
            "\"other\", \"cities-deadbeef\"",
            "W/\"cities-deadbeef\"",
            "*",
        ] {
            let value = HeaderValue::from_str(raw)?;
            assert!(etag_matches(Some(&value), expected), "{raw}");
        }
        assert!(!etag_matches(
            Some(&HeaderValue::from_static("\"other\"")),
            expected
        ));
        Ok(())
    }

    #[test]
    fn referral_url_uses_the_configured_first_party_origin()
    -> Result<(), Box<dyn std::error::Error>> {
        let base = Url::parse("https://virya.music/")?;
        let code = ReferralCode::parse("safe_Code-123")?;

        assert_eq!(
            referral_url(&base, &code)?,
            "https://virya.music/r/safe_Code-123"
        );
        Ok(())
    }
}
