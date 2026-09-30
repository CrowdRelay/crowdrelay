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
