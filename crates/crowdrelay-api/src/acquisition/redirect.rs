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
    // Cache hits stay allocation-free and database-free. A freshly published
    // link can legitimately miss this periodically refreshed snapshot for one
    // refresh interval, though, and returning 404 in that window loses the
    // first human who clicks the post. On a miss do one tenant-scoped indexed
    // read before deciding the slug does not exist.
    let snapshot = state.acquisition.redirect_cache.snapshot();
    let link = match snapshot
        .resolve(state.acquisition.workspace_id, &slug)
        .cloned()
    {
        Some(link) => link,
        None => match state
            .acquisition
            .acquisition_repository
            .load_active_smart_link(state.acquisition.workspace_id, &slug)
            .await
        {
            Ok(Some(link)) => {
                tracing::debug!(
                    smart_link_id = %link.id(),
                    slug = %slug.as_str(),
                    "resolved fresh smart link from database after redirect-cache miss"
                );
                link
            }
            Ok(None) => {
                return Problem::not_found(request_id(&headers))
                    .private()
                    .into_response();
            }
            Err(error) => {
                // A cache miss plus an unavailable repository is ambiguous:
                // the link may have been minted after the last snapshot.
                // 503 is retryable and truthful; a 404 would permanently tell
                // a real first visitor that a valid fresh link does not exist.
                tracing::warn!(
                    %error,
                    slug = %slug.as_str(),
                    "smart-link cache miss could not be resolved from repository"
                );
                return Problem::service_unavailable(request_id(&headers))
                    .private()
                    .into_response();
            }
        },
    };

    // Previews, crawlers, HEAD probes and prefetches get the redirect but are
    // not an audience: no click row, no attribution cookie (see
    // `automated_fetch`). The visitor id is still minted so the response shape
    // is identical for every caller.
    let automated = automated_fetch(&method, &headers);
    let visitor_id = attribution_visitor(&headers).unwrap_or_default();
    if let Some(reason) = automated {
        record_dropped(reason);
        tracing::debug!(smart_link_id = %link.id(), "tracked-link fetch by an automated agent not recorded as a click");
    } else {
        let occurred_at = OffsetDateTime::now_utc();
        let referrer_host = referrer_host(&headers);
        let event = match ClickEvent::from_link(
            &link,
            Some(visitor_id),
            referrer_host,
            occurred_at,
        ) {
            Ok(event) => event,
            Err(error) => {
                // Referrer is optional metadata, never the click itself. A
                // malformed external Referer header must not erase a real
                // human interaction. Retry the same observed click with no
                // referrer rather than sending an unprovable visitor onward.
                tracing::debug!(%error, "discarded invalid click referrer metadata");
                match ClickEvent::from_link(&link, Some(visitor_id), None, occurred_at) {
                    Ok(event) => event,
                    Err(error) => {
                        tracing::error!(%error, "failed to construct click without optional referrer");
                        return Problem::internal(request_id(&headers))
                            .private()
                            .into_response();
                    }
                }
            }
        };
        if (state.acquisition.click_submitter)(event).await
            == ClickSubmission::Unavailable
        {
            tracing::warn!(
                smart_link_id = %link.id(),
                "human tracked click could not reach a durable ingestion path"
            );
            return Problem::service_unavailable(request_id(&headers))
                .private()
                .into_response();
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
    if automated.is_none() {
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
