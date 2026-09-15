// The scan view: the one thing a band member needs on a phone at the door —
// the scannable check-in URL for the night's live campaign, plus the live
// count so the page doubles as the tally board.
//
// The URL carries the campaign's HMAC token in its fragment — the same
// credential the staff print tool renders. It stays off the list and
// timeline responses deliberately: this endpoint exists to hand it to the
// person holding the door, on demand, nothing else.

/// The fan-site path the QR points at — the same `/pl/live/...` the staff
/// print tool produces for the first tenant, so a phone scan and a poster
/// scan land on the same page. A tenant whose fan site defaults to another
/// locale changes the convention here, not by forking the URL shape.
const PUBLIC_LIVE_PATH_PREFIX: &str = "pl/live";

#[derive(Debug, FromRow)]
struct ScanCampaignRow {
    id: Uuid,
    event_id: Uuid,
    label: String,
    valid_from: OffsetDateTime,
    valid_until: OffsetDateTime,
    max_checkins: Option<i32>,
    active: bool,
    revoked_at: Option<OffsetDateTime>,
    checkin_count: i64,
}

#[derive(Debug, Serialize)]
struct ControlPlaneEventScanResponse {
    /// The scannable URL — `{public_site}/pl/live/{slug}/#checkin={token}` —
    /// or null when no campaign is live for the night. A null here is a fact
    /// (nothing to scan yet), not an error.
    checkin_url: Option<String>,
    campaign_label: Option<String>,
    /// The window edges the door needs: before `valid_from` the same QR is a
    /// dead scan, so the page says "goes live at" rather than handing out a
    /// code the door will reject.
    valid_from: Option<String>,
    valid_until: Option<String>,
    /// The night's tally across all of its campaigns — the number the door
    /// watches. `campaign_checkin_count` is the live campaign's own slice,
    /// the value `max_checkins` actually caps.
    checkin_count: i64,
    campaign_checkin_count: Option<i64>,
    max_checkins: Option<u32>,
}

/// `GET /v1/control-plane/events/{event_slug}/scan` — the door view.
pub async fn control_plane_event_scan(
    State(state): State<crate::AppState>,
    Path(event_slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    match crate::ops::hold(
        &state.read_budget,
        load_scan_facts(&state.concert_qr, &event_slug),
    )
    .await
    {
        Ok(Some(facts)) => {
            // A live campaign that cannot produce a URL is a fault, not a
            // null — the door reading "no campaign" while one exists would
            // create a duplicate and hit the same wall.
            let checkin_url = if facts.campaign_is_live {
                match facts.token.and_then(|token| {
                    state
                        .acquisition
                        .public_site_base_url()
                        .join(&format!(
                            "{PUBLIC_LIVE_PATH_PREFIX}/{}/#checkin={token}",
                            facts.event_slug
                        ))
                        .map(|url| url.to_string())
                        .ok()
                }) {
                    Some(url) => Some(url),
                    None => {
                        return Problem::service_unavailable(request_id_value)
                            .private()
                            .into_response()
                    }
                }
            } else {
                None
            };
            (
                StatusCode::OK,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(ControlPlaneEventScanResponse {
                    checkin_url,
                    campaign_label: facts.label,
                    valid_from: facts.valid_from.map(format_time),
                    valid_until: facts.valid_until.map(format_time),
                    checkin_count: facts.checkin_count,
                    campaign_checkin_count: facts.campaign_checkin_count,
                    max_checkins: facts.max_checkins,
                }),
            )
                .into_response()
        }
        Ok(None) => Problem::not_found(request_id_value)
            .private()
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "control-plane event scan query failed");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}

struct ScanFacts {
    event_slug: String,
    campaign_is_live: bool,
    token: Option<String>,
    label: Option<String>,
    valid_from: Option<OffsetDateTime>,
    valid_until: Option<OffsetDateTime>,
    checkin_count: i64,
    campaign_checkin_count: Option<i64>,
    max_checkins: Option<u32>,
}

/// The campaign the door uses: the staff manager's own pick — the first
/// active, unrevoked, unexpired campaign for the event — plus the running
/// check-in count across all of the night's campaigns.
async fn load_scan_facts(
    state: &ConcertQrState,
    event_slug: &str,
) -> Result<Option<ScanFacts>, sqlx::Error> {
    let workspace_id = state.workspace_id.into_uuid();
    let Some(event) = sqlx::query_as::<_, TimelineEventRow>(
        r#"
        SELECT id, slug, title, venue, status, starts_at, ends_at
        FROM events
        WHERE workspace_id = $1 AND slug = $2
          AND status IN ('published','completed')
        "#,
    )
    .bind(workspace_id)
    .bind(event_slug)
    .fetch_optional(&state.database)
    .await?
    else {
        return Ok(None);
    };

    let campaign = sqlx::query_as::<_, ScanCampaignRow>(
        r#"
        SELECT campaign.id, campaign.event_id, campaign.label,
               campaign.valid_from, campaign.valid_until, campaign.max_checkins,
               campaign.active, campaign.revoked_at,
               count(checkin.id)::bigint AS checkin_count
        FROM concert_qr_campaigns AS campaign
        LEFT JOIN concert_checkins AS checkin
          ON checkin.workspace_id = campaign.workspace_id
         AND checkin.campaign_id = campaign.id
        WHERE campaign.workspace_id = $1 AND campaign.event_id = $2
        GROUP BY campaign.id
        ORDER BY campaign.created_at DESC, campaign.id DESC
        LIMIT 50
        "#,
    )
    .bind(workspace_id)
    .bind(event.id)
    .fetch_all(&state.database)
    .await?;

    let total_checkins: i64 = campaign.iter().map(|row| row.checkin_count).sum();
    // Same effective-active rule as `campaign_view`: the window's open edge
    // is deliberately NOT part of the pick — the print tool hands out the
    // same code days ahead — but it IS part of the response so the page can
    // say when the door actually opens.
    let live = campaign.into_iter().find(|row| {
        row.active && row.revoked_at.is_none() && row.valid_until > OffsetDateTime::now_utc()
    });
    let token = live.as_ref().and_then(|row| {
        state
            .signing_key
            .as_ref()
            .and_then(|key| sign_token(row.id, row.event_id, row.valid_until, key))
    });
    Ok(Some(ScanFacts {
        event_slug: event.slug,
        campaign_is_live: live.is_some(),
        token,
        label: live.as_ref().map(|row| row.label.clone()),
        valid_from: live.as_ref().map(|row| row.valid_from),
        valid_until: live.as_ref().map(|row| row.valid_until),
        checkin_count: total_checkins,
        campaign_checkin_count: live.as_ref().map(|row| row.checkin_count),
        max_checkins: live
            .as_ref()
            .and_then(|row| row.max_checkins.and_then(|v| u32::try_from(v).ok())),
    }))
}
