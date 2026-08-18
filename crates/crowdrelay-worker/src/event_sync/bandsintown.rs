// Bandsintown provider adapter: payload shapes, timestamp resolution and
// normalization into the sync's own event model. Kept out of event_sync.rs
// so the orchestration there stays about leasing, persisting and
// announcing rather than one provider's wire format.

impl EventSyncWorker {
    async fn fetch_bandsintown(
        &self,
        source: &EventSourceRow,
    ) -> Result<ProviderBatch, EventSyncError> {
        let app_id =
            resolve_bandsintown_app_id(self.bandsintown_api_key.as_deref(), source.app_id.as_str());
        let mut url = Url::parse(&format!(
            "https://rest.bandsintown.com/artists/{}/events",
            encode_path_segment(&source.artist_name)
        ))
        .map_err(|_| EventSyncError::InvalidSource)?;
        url.query_pairs_mut()
            .append_pair("app_id", app_id)
            .append_pair("date", "upcoming");

        let response = self
            .client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|_| EventSyncError::ProviderUnavailable)?;
        if matches!(response.status().as_u16(), 401 | 403) {
            return Err(EventSyncError::ProviderAuthentication(
                response.status().as_u16(),
            ));
        }
        if !response.status().is_success() {
            return Err(EventSyncError::ProviderStatus(response.status().as_u16()));
        }

        let body = read_limited_body(response).await?;
        let payload = serde_json::from_slice::<Vec<BandsintownEvent>>(&body)
            .map_err(|_| EventSyncError::InvalidProviderPayload)?;
        if payload.len() > MAX_PROVIDER_EVENTS {
            return Err(EventSyncError::ProviderPayloadTooLarge);
        }

        // A single unparseable event used to abort the whole calendar: `collect`
        // into a `Result` short-circuits, so one bad timestamp meant zero gigs
        // persisted and the source retried forever. Skip the bad rows instead,
        // and report the batch as incomplete so reconciliation stays disarmed.
        let mut events = Vec::with_capacity(payload.len());
        let mut skipped = Vec::new();
        for event in payload {
            // Reuse the provider-id normalizer so the diagnostic names the event
            // the same way the rest of the sync does; an id too malformed to
            // normalize is still worth reporting verbatim.
            let raw_id = external_id(&event.id).unwrap_or_else(|| event.id.to_string());
            match normalize_bandsintown_event(event, source) {
                Ok(event) => events.push(event),
                Err(error) => {
                    tracing::warn!(
                        source_id = %source.id,
                        provider = %source.provider,
                        artist = %source.artist_name,
                        provider_event_id = %raw_id,
                        error = %error,
                        "skipping unparseable provider event"
                    );
                    skipped.push(raw_id);
                }
            }
        }
        Ok(ProviderBatch { events, skipped })
    }
}


/// Events accepted from a provider response, plus the ones that could not be
/// normalized. A batch that skipped anything is not a complete view of the
/// calendar, so it must not be used to cancel events it simply failed to read.
struct ProviderBatch {
    events: Vec<NormalizedExternalEvent>,
    skipped: Vec<String>,
}

impl ProviderBatch {
    fn is_complete(&self) -> bool {
        self.skipped.is_empty()
    }
}

#[derive(Debug, Deserialize)]
struct BandsintownEvent {
    id: serde_json::Value,
    url: Option<String>,
    datetime: String,
    title: Option<String>,
    description: Option<String>,
    lineup: Option<Vec<String>>,
    venue: BandsintownVenue,
    offers: Option<Vec<BandsintownOffer>>,
}

#[derive(Debug, Deserialize)]
struct BandsintownVenue {
    name: Option<String>,
    city: Option<String>,
    region: Option<String>,
    country: Option<String>,
    location: Option<String>,
    latitude: Option<String>,
    longitude: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BandsintownOffer {
    #[serde(rename = "type")]
    offer_type: Option<String>,
    url: Option<String>,
    status: Option<String>,
}

fn parse_bandsintown_datetime(
    value: &str,
    timezone: &str,
) -> Result<OffsetDateTime, EventSyncError> {
    // Accept an explicit provider offset if Bandsintown starts returning one.
    if let Ok(datetime) = OffsetDateTime::parse(value, &Rfc3339) {
        return Ok(datetime);
    }

    // Bandsintown currently returns artist-event timestamps as local ISO 8601
    // wall-clock time, e.g. `2026-09-11T20:00:00`, without a UTC offset.
    // Resolve that value through the source's IANA timezone so DST is correct.
    let local = PrimitiveDateTime::parse(value, &Iso8601::DEFAULT)
        .map_err(|_| EventSyncError::InvalidProviderPayload)?;
    let timezone = timezones::get_by_name(timezone).ok_or(EventSyncError::InvalidSource)?;

    match local.assume_timezone(timezone) {
        OffsetResult::Some(datetime) => Ok(datetime),
        // Never guess during a DST fold or gap.
        OffsetResult::Ambiguous(_, _) | OffsetResult::None => {
            Err(EventSyncError::InvalidProviderPayload)
        }
    }
}

fn normalize_bandsintown_event(
    event: BandsintownEvent,
    source: &EventSourceRow,
) -> Result<NormalizedExternalEvent, EventSyncError> {
    let source_event_id = external_id(&event.id).ok_or(EventSyncError::InvalidProviderPayload)?;
    let starts_at = parse_bandsintown_datetime(&event.datetime, &source.timezone)?;
    let city_name = clean_optional(event.venue.city);
    let country_code = country_code(event.venue.country.as_deref(), &source.default_country_code);
    let city_slug = city_name
        .as_deref()
        .map(|name| stable_slug(name, &country_code));
    let lineup = event.lineup.unwrap_or_default();
    let title = clean_optional(event.title)
        .or_else(|| {
            let names: Vec<_> = lineup
                .into_iter()
                .filter_map(|name| clean_optional(Some(name)))
                .collect();
            (!names.is_empty()).then(|| names.join(" · "))
        })
        .unwrap_or_else(|| match city_name.as_deref() {
            Some(city) => format!("{} live — {city}", source.artist_name),
            None => format!("{} live", source.artist_name),
        });
    let ticket_url = event
        .offers
        .unwrap_or_default()
        .into_iter()
        .find(|offer| {
            offer.url.is_some()
                && offer.status.as_deref() != Some("cancelled")
                && offer
                    .offer_type
                    .as_deref()
                    .is_none_or(|kind| kind.eq_ignore_ascii_case("tickets"))
        })
        .and_then(|offer| valid_public_https_url(offer.url));

    Ok(NormalizedExternalEvent {
        slug: format!(
            "gig-{}-{}",
            source.id.simple(),
            stable_slug(&source_event_id, "event")
        ),
        source_event_id,
        title,
        description: clean_optional(event.description),
        city_name,
        city_slug,
        country_code,
        region: clean_optional(event.venue.region),
        latitude: event
            .venue
            .latitude
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| (-90.0..=90.0).contains(value)),
        longitude: event
            .venue
            .longitude
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| (-180.0..=180.0).contains(value)),
        venue: clean_optional(event.venue.name),
        venue_address: clean_optional(event.venue.location),
        timezone: source.timezone.clone(),
        starts_at,
        ticket_url,
        external_event_url: valid_public_https_url(event.url),
    })
}

fn resolve_bandsintown_app_id<'a>(
    configured_api_key: Option<&'a str>,
    source_app_id: &'a str,
) -> &'a str {
    configured_api_key.unwrap_or(source_app_id)
}
