/// Resolves a smart link, records non-critical attribution, and redirects immediately.
pub async fn redirect_smart_link(
    State(state): State<crate::AppState>,
    Path(raw_slug): Path<String>,
    method: Method,
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

    // Previews, crawlers, HEAD probes and prefetches get the redirect but are
    // not an audience: no click row, no attribution cookie (see
    // `automated_fetch`). The visitor id is still minted so the response shape
    // is identical for every caller.
    let automated = is_automated_fetch(&method, &headers);
    let visitor_id = attribution_visitor(&headers).unwrap_or_default();
    if automated {
        tracing::debug!(smart_link_id = %link.id(), "tracked-link fetch by an automated agent not recorded as a click");
    } else {
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
    }

    // A configured capture origin may interpose its /watch/{id} page between
    // the click and YouTube — only for owned videos on channels that allow a
    // landing. Everything else keeps the destination byte for byte.
    let destination = state
        .acquisition
        .watch_origin
        .as_ref()
        .and_then(|origin| {
            landing_for(
                LandingInputs {
                    destination: link.destination_url().as_str(),
                    channel: link.channel_source(),
                    community: link.channel_community(),
                },
                |id| snapshot.owns_video(state.acquisition.workspace_id, id),
                |community| {
                    snapshot.reddit_offsite_allowed(state.acquisition.workspace_id, community)
                },
                origin.as_str().trim_end_matches('/'),
            )
        })
        .unwrap_or_else(|| link.destination_url().as_str().to_owned());

    let Ok(location) = HeaderValue::from_str(&destination) else {
        tracing::error!(
            smart_link_id = %link.id(),
            "validated smart-link destination could not be encoded as a response header"
        );
        return Problem::internal(request_id(&headers))
            .private()
            .into_response();
    };
    let mut response = (
        StatusCode::FOUND,
        [
            (LOCATION, location),
            (CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE)),
            (REFERRER_POLICY, HeaderValue::from_static("no-referrer")),
        ],
    )
        .into_response();
    if !automated {
        let Ok(cookie) = HeaderValue::from_str(&attribution_cookie(
            visitor_id,
            state.acquisition.secure_cookies,
        )) else {
            tracing::error!("attribution cookie could not be encoded as a response header");
            return Problem::internal(request_id(&headers))
                .private()
                .into_response();
        };
        response.headers_mut().insert(SET_COOKIE, cookie);
    }
    response
}
