// The show kit (FAN_100 §B5): one QR campaign per printed placement
// plus a tracked link for each act sharing the bill, in a single call.

/// The placements one show's QR kit always prints (FAN_100 §B5): the scan
/// rate only means something read per placement, so the kit creates all
/// three rather than trusting whoever sets the room up to invent labels
/// that match later.
const QR_KIT_PLACEMENTS: [(&str, &str); 3] = [
    ("door", "Door"),
    ("stage-screen", "Stage screen"),
    ("merch-table", "Merch table"),
];

/// How far before the posted start a kit QR goes live — the poster is up
/// during soundcheck, not a week early. Inside the `EARLIEST_BEFORE_EVENT`
/// bound either way.
const KIT_VALID_BEFORE_EVENT: Duration = Duration::hours(6);
/// Scans happen until the room empties; the kit stays live half a day past
/// the announced start, well inside `LATEST_AFTER_EVENT`.
const KIT_VALID_AFTER_EVENT: Duration = Duration::hours(12);
const MAX_BILL_ACTS: usize = 6;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateQrKitRequest {
    event_slug: String,
    /// The other acts on the bill. Each gets a `bill-{act}` tracked link to
    /// the same show page, so a furydate-share click is measured as theirs —
    /// the bill is three bands' audiences, not one.
    #[serde(default)]
    bill_acts: Vec<String>,
    /// What the scan offers, written onto every placement the kit creates.
    incentive: Option<String>,
}

#[derive(Debug, Serialize)]
struct BillLinkView {
    act: String,
    slug: String,
    url: String,
    destination_url: String,
}

#[derive(Debug, Serialize)]
struct QrKitResponse {
    event_slug: String,
    campaigns: Vec<CampaignView>,
    bill_links: Vec<BillLinkView>,
}

#[derive(Debug, FromRow)]
struct KitEventRow {
    id: Uuid,
    slug: String,
    title: String,
    starts_at: OffsetDateTime,
}

/// `POST /v1/admin/event-qr/kits` — one show's complete scan surface in a
/// single call: the three canonical placements, created where they do not
/// already exist and returned where they do, plus a `bill-{act}` link for
/// each act sharing the bill. Idempotent on (event, placement) so running
/// the kit twice repairs nothing and duplicates nothing.
pub async fn create_qr_kit(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreateQrKitRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Some(signing_key) = state.concert_qr.signing_key else {
        return Problem::service_unavailable(request_id_value)
            .private()
            .into_response();
    };
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    let Ok(event_slug) = EventSlug::parse(payload.event_slug.trim()) else {
        return Problem::unprocessable(request_id_value)
            .private()
            .into_response();
    };
    let incentive = bounded_optional_text(payload.incentive.as_deref(), 256);
    if payload.incentive.is_some() && incentive.is_none() {
        return Problem::unprocessable(request_id_value)
            .private()
            .into_response();
    }

    // Normalize each act name into the slug tail it will be shared as —
    // `bill-furydate`, `bill-impala`. Anything that cannot become a clean
    // slug is refused up front rather than minted mangled.
    if payload.bill_acts.len() > MAX_BILL_ACTS {
        return Problem::unprocessable(request_id_value)
            .private()
            .into_response();
    }
    let mut bill_acts = Vec::with_capacity(payload.bill_acts.len());
    for act in &payload.bill_acts {
        let Some(normalized) = crowdrelay_domain::slugify(act) else {
            return Problem::unprocessable(request_id_value)
                .private()
                .into_response();
        };
        let Ok(slug) = SmartLinkSlug::parse(format!("bill-{normalized}")) else {
            return Problem::unprocessable(request_id_value)
                .private()
                .into_response();
        };
        bill_acts.push((normalized, slug));
    }

    let event = match sqlx::query_as::<_, KitEventRow>(
        r#"
        SELECT id, slug, title, starts_at
        FROM events
        WHERE workspace_id = $1 AND slug = $2 AND status = 'published'
        "#,
    )
    .bind(state.concert_qr.workspace_id.into_uuid())
    .bind(event_slug.as_str())
    .fetch_optional(&state.concert_qr.database)
    .await
    {
        Ok(Some(row)) => row,
        Ok(None) => {
            return Problem::not_found(request_id_value)
                .private()
                .into_response();
        }
        Err(_) => {
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };

    // Bill links need somewhere to land; the QR half works without a site.
    // Refuse only when acts were actually asked for — a kit missing its bill
    // links silently is a gap nobody sees until the other bands' posts are
    // already up.
    let brand = match TenantSettingsRepository::new(state.database.clone())
        .brand_settings(state.concert_qr.workspace_id.into_uuid())
        .await
    {
        Ok(brand) => brand,
        Err(error) => {
            tracing::warn!(%error, "qr-kit brand settings load failed");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };
    let show_destination = brand.site_root().map(|root| {
        format!(
            "{}/{}/{}",
            root,
            brand.live_page_path.trim_matches('/'),
            event.slug
        )
    });
    if !bill_acts.is_empty() && show_destination.is_none() {
        return Problem::unprocessable(request_id_value)
            .private()
            .into_response();
    }

    // A show whose window has fully ended keeps its campaigns as the
    // historical record; minting fresh codes nobody can scan would split
    // the placement readout across dead and live rows alike.
    if event.starts_at + KIT_VALID_AFTER_EVENT <= OffsetDateTime::now_utc() {
        return Problem::unprocessable(request_id_value)
            .private()
            .into_response();
    }

    // Existing campaigns for this event, so a placement already printed is
    // handed back rather than doubled.
    let existing = match sqlx::query_as::<_, CampaignRow>(
        r#"
        SELECT campaign.id, campaign.event_id, event.slug AS event_slug,
               event.title AS event_title, event.venue, event.starts_at,
               campaign.label, campaign.valid_from, campaign.valid_until,
               campaign.max_checkins, campaign.placement,
               campaign.announced_from_stage, campaign.incentive,
               campaign.active, campaign.revoked_at,
               campaign.created_at, count(checkin.id)::bigint AS checkin_count
        FROM concert_qr_campaigns AS campaign
        INNER JOIN events AS event
          ON event.workspace_id = campaign.workspace_id AND event.id = campaign.event_id
        LEFT JOIN concert_checkins AS checkin
          ON checkin.workspace_id = campaign.workspace_id AND checkin.campaign_id = campaign.id
        WHERE campaign.workspace_id = $1
          AND campaign.event_id = $2
          AND campaign.active
          AND campaign.revoked_at IS NULL
        GROUP BY campaign.id, event.id
        "#,
    )
    .bind(state.concert_qr.workspace_id.into_uuid())
    .bind(event.id)
    .fetch_all(&state.concert_qr.database)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "qr-kit campaign listing failed");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };

    let now = OffsetDateTime::now_utc();
    let mut campaigns: Vec<CampaignView> = Vec::with_capacity(QR_KIT_PLACEMENTS.len());
    for (placement, label) in QR_KIT_PLACEMENTS {
        if let Some(row) = existing
            .iter()
            .find(|row| row.placement.as_deref() == Some(placement))
        {
            campaigns.push(campaign_view_ref(row, Some(&signing_key)));
            continue;
        }
        let command = CreateCampaignCommand {
            workspace_id: state.concert_qr.workspace_id.into_uuid(),
            event_slug: event_slug.as_str().to_owned(),
            label: format!("{label} — {}", event.title),
            valid_from: event.starts_at - KIT_VALID_BEFORE_EVENT,
            valid_until: event.starts_at + KIT_VALID_AFTER_EVENT,
            max_checkins: None,
            placement: Some(placement.to_owned()),
            announced_from_stage: false,
            incentive: incentive.clone(),
            created_at: now,
            request_id: request_id_value.clone(),
        };
        let result = match state.concert_qr_repo.create_campaign(&command).await {
            Ok(value) => value,
            Err(ConcertQrError::NotFound) => {
                return Problem::not_found(request_id_value)
                    .private()
                    .into_response();
            }
            Err(ConcertQrError::Conflict) => {
                return Problem::conflict(request_id_value)
                    .private()
                    .into_response();
            }
            Err(ConcertQrError::Invalid) => {
                return Problem::unprocessable(request_id_value)
                    .private()
                    .into_response();
            }
            Err(ConcertQrError::Unavailable) => {
                return Problem::service_unavailable(request_id_value)
                    .private()
                    .into_response();
            }
        };
        campaigns.push(campaign_view(
            CampaignRow {
                id: result.campaign_id,
                event_id: result.event.id,
                event_slug: result.event.slug,
                event_title: result.event.title,
                venue: result.event.venue,
                starts_at: result.event.starts_at,
                label: command.label.clone(),
                valid_from: command.valid_from,
                valid_until: command.valid_until,
                max_checkins: command.max_checkins,
                placement: command.placement.clone(),
                announced_from_stage: command.announced_from_stage,
                incentive: command.incentive.clone(),
                active: true,
                revoked_at: None,
                created_at: result.created_at,
                checkin_count: 0,
            },
            Some(&signing_key),
        ));
    }

    let mut bill_links = Vec::with_capacity(bill_acts.len());
    if let Some(destination) = show_destination {
        for (act, slug) in bill_acts {
            let command = UpsertSmartLinkCommand {
                workspace_id: state.concert_qr.workspace_id,
                slug: &slug,
                destination_url: &destination,
                channel_source: Some("bill"),
                channel_community: Some(&act),
                channel_creative: None,
                campaign_id: None,
            };
            match state
                .acquisition
                .acquisition_repository()
                .upsert_smart_link(&command)
                .await
            {
                Ok(link) => bill_links.push(BillLinkView {
                    act,
                    url: format!(
                        "{}/l/{}",
                        brand.site_root().unwrap_or_default().trim_end_matches('/'),
                        link.slug.as_str()
                    ),
                    slug: link.slug.into_inner(),
                    destination_url: link.destination_url,
                }),
                Err(error) => {
                    tracing::warn!(%error, %slug, "qr-kit bill link mint failed");
                    return Problem::service_unavailable(request_id_value)
                        .private()
                        .into_response();
                }
            }
        }
    }

    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(QrKitResponse {
            event_slug: event_slug.as_str().to_owned(),
            campaigns,
            bill_links,
        }),
    )
        .into_response()
}

/// `campaign_view` for a borrowed row — the kit re-renders existing
/// placements without consuming them.
fn campaign_view_ref(row: &CampaignRow, signing_key: Option<&[u8; 32]>) -> CampaignView {
    let effective_active =
        row.active && row.revoked_at.is_none() && row.valid_until > OffsetDateTime::now_utc();
    let token = if effective_active {
        signing_key.and_then(|key| sign_token(row.id, row.event_id, row.valid_until, key))
    } else {
        None
    };
    CampaignView {
        id: row.id,
        event_id: row.event_id,
        event_slug: row.event_slug.clone(),
        event_title: row.event_title.clone(),
        venue: row.venue.clone(),
        starts_at: format_time(row.starts_at),
        label: row.label.clone(),
        valid_from: format_time(row.valid_from),
        valid_until: format_time(row.valid_until),
        max_checkins: row.max_checkins.and_then(|value| u32::try_from(value).ok()),
        checkin_count: u64::try_from(row.checkin_count).unwrap_or_default(),
        placement: row.placement.clone(),
        announced_from_stage: row.announced_from_stage,
        incentive: row.incentive.clone(),
        active: effective_active,
        revoked_at: row.revoked_at.map(format_time),
        created_at: format_time(row.created_at),
        token,
    }
}
