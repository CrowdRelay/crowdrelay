//! Provider adapters for the platforms that need no OAuth dance.
//!
//! Discord reads a free public API, Telegram carries its bot token on the
//! connection row, Last.fm uses a shared process-level API key, Deezer and
//! Bluesky expose free public endpoints, and Discogs uses a shared personal
//! access token. None of them needs a token refresh, so they share nothing
//! with the OAuth adapters in the parent module beyond `record_metric_point`.
//!
//! Each one errors rather than recording a zero when the provider answers
//! without a count: points hold absolute levels, so a fabricated 0 reads as
//! the audience vanishing and poisons every trend derived from the series.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use crowdrelay_infra::sensitive_response::decrypt_value;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{
    DueConnection, GrowthMetricSyncError, GrowthMetricSyncWorker, record_metric_point, urlencode,
};

/// AAD for Telegram bot token encryption. Same pattern as TikTok, and it must
/// match `PostgresFanbaseRepository::token_aad` or the token will not decrypt.
fn telegram_bot_aad(workspace_id: Uuid, channel: &str) -> Vec<u8> {
    format!("crowdrelay.fanbase.oauth.telegram.v1\0{workspace_id}\0{channel}").into_bytes()
}

impl GrowthMetricSyncWorker {
    /// Discord: fetch server member count from Discord's own invite API
    /// (free, no API key). The `external_account_ref` column stores the
    /// Discord invite code (e.g. `BBdDV6gVy`). When posting is configured,
    /// `provider_account_id` holds the channel ID (a numeric snowflake),
    /// so the sync reader must use `external_account_ref` — the one column
    /// that always carries the invite code.
    pub(super) async fn sync_discord(
        &self,
        conn: &DueConnection,
    ) -> Result<(), GrowthMetricSyncError> {
        let invite_code = &conn.external_account_ref;
        let url = format!("https://discord.com/api/v9/invites/{invite_code}?with_counts=true");
        let response = self.http_client.get(&url).send().await?;
        if !response.status().is_success() {
            return Err(GrowthMetricSyncError::ProviderApi(format!(
                "discord.com invite API returned HTTP {} for invite code {invite_code}",
                response.status()
            )));
        }
        let body: serde_json::Value = response.json().await?;
        // A missing count is an error, not a zero: recording 0 would land an
        // absolute level in the series and read as the server losing every
        // member. Same rule as every other platform in this worker.
        let member_count = body
            .get("approximate_member_count")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| {
                GrowthMetricSyncError::ProviderApi(format!(
                    "discord.com invite API returned no member count for invite code {invite_code}"
                ))
            })?;
        let name = body
            .get("guild")
            .and_then(|g| g.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("Discord server");
        record_metric_point(
            &self.pool,
            conn.workspace_id,
            conn.id,
            "discord",
            "members",
            &format!("Discord members — {name}"),
            member_count,
            OffsetDateTime::now_utc(),
        )
        .await?;
        tracing::info!(
            connection_id = %conn.id,
            invite_code = %invite_code,
            members = member_count,
            "discord server member count recorded"
        );
        Ok(())
    }

    /// Telegram: fetch channel subscriber count via the Bot API
    /// (getChatMemberCount). The `provider_account_id` stores the channel
    /// username (e.g. `@virya_music`). The bot token is stored encrypted
    /// in `encrypted_access_token`.
    pub(super) async fn sync_telegram(
        &self,
        conn: &DueConnection,
    ) -> Result<(), GrowthMetricSyncError> {
        let channel = &conn.provider_account_id;

        // Read encrypted bot token from the connection (same pattern as TikTok).
        let row: (Option<String>,) = sqlx::query_as(
            r#"SELECT encrypted_access_token
               FROM fanbase_connections WHERE id = $1"#,
        )
        .bind(conn.id)
        .fetch_one(&self.pool)
        .await?;

        let encrypted_token = row.0.ok_or_else(|| {
            GrowthMetricSyncError::ProviderApi(
                "Telegram connection missing encrypted_access_token (bot token)".to_string(),
            )
        })?;

        // Decrypt the bot token.
        let aad = telegram_bot_aad(conn.workspace_id, channel);
        let token_bytes = URL_SAFE_NO_PAD.decode(&encrypted_token).map_err(|_| {
            GrowthMetricSyncError::ProviderApi("Telegram bot token is not valid base64".to_string())
        })?;
        let bot_token = String::from_utf8(
            decrypt_value(&token_bytes, &self.response_encryption_key, &aad).map_err(|e| {
                GrowthMetricSyncError::ProviderApi(format!(
                    "Telegram bot token decryption failed: {e}"
                ))
            })?,
        )
        .map_err(|_| {
            GrowthMetricSyncError::ProviderApi("Telegram bot token is not valid UTF-8".to_string())
        })?;

        // The bot token is a path segment, so it is inside the request URL.
        // reqwest's Display includes the URL, and these errors are logged —
        // strip the URL off every error out of this call so the token cannot
        // reach the log. Same reason the response body is not logged raw.
        let url = format!(
            "https://api.telegram.org/bot{bot_token}/getChatMemberCount?chat_id={}",
            urlencode(channel)
        );
        let response = self
            .http_client
            .get(&url)
            .send()
            .await
            .map_err(|error| GrowthMetricSyncError::Http(error.without_url()))?;
        if !response.status().is_success() {
            return Err(GrowthMetricSyncError::ProviderApi(format!(
                "Telegram Bot API returned HTTP {} for {channel}",
                response.status()
            )));
        }
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|error| GrowthMetricSyncError::Http(error.without_url()))?;
        // `ok: false` comes back with HTTP 200 in some Bot API deployments, and
        // a missing count must not be recorded as a zero level — that would
        // read as the channel losing every subscriber.
        let subscriber_count = body
            .get("result")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| {
                GrowthMetricSyncError::ProviderApi(format!(
                    "Telegram Bot API returned no member count for {channel}"
                ))
            })?;
        record_metric_point(
            &self.pool,
            conn.workspace_id,
            conn.id,
            "telegram",
            "subscribers",
            &format!("Telegram subscribers — {channel}"),
            subscriber_count,
            OffsetDateTime::now_utc(),
        )
        .await?;
        tracing::info!(
            connection_id = %conn.id,
            channel = %channel,
            subscribers = subscriber_count,
            "telegram channel subscriber count recorded"
        );
        Ok(())
    }

    /// Sync Last.fm listener count for an artist.
    ///
    /// Uses the official Last.fm API `artist.getInfo` endpoint. The artist
    /// name is stored in `provider_account_id`. The API key is a shared env
    /// var (`CROWDRELAY_LASTFM_API_KEY`) — no per-connection secret needed.
    pub(super) async fn sync_lastfm(
        &self,
        conn: &DueConnection,
    ) -> Result<(), GrowthMetricSyncError> {
        let api_key = self.lastfm_api_key.as_ref().ok_or_else(|| {
            GrowthMetricSyncError::ProviderApi(
                "Last.fm API key not configured (CROWDRELAY_LASTFM_API_KEY)".to_string(),
            )
        })?;
        let artist = &conn.provider_account_id;

        let url = format!(
            "https://ws.audioscrobbler.com/2.0/?method=artist.getinfo&artist={}&api_key={api_key}&format=json",
            urlencode(artist)
        );
        // The API key is a query parameter, so it is inside the request URL and
        // reqwest's Display would carry it into the log. Strip the URL off.
        let response = self
            .http_client
            .get(&url)
            .send()
            .await
            .map_err(|error| GrowthMetricSyncError::Http(error.without_url()))?;
        if !response.status().is_success() {
            return Err(GrowthMetricSyncError::ProviderApi(format!(
                "Last.fm API returned HTTP {} for artist '{artist}'",
                response.status()
            )));
        }
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|error| GrowthMetricSyncError::Http(error.without_url()))?;
        // Last.fm answers an unknown artist with HTTP 200 and an `error` body.
        // Both stats are absolute levels, so a missing one is an error rather
        // than a zero — recording 0 would read as the artist losing every
        // listener and would poison the trend.
        let stats = body.get("artist").and_then(|a| a.get("stats"));
        let stat = |key: &str| -> Result<i64, GrowthMetricSyncError> {
            stats
                .and_then(|s| s.get(key))
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<i64>().ok())
                .ok_or_else(|| {
                    GrowthMetricSyncError::ProviderApi(format!(
                        "Last.fm API returned no {key} for artist '{artist}'"
                    ))
                })
        };
        let listeners = stat("listeners")?;
        let playcount = stat("playcount")?;

        record_metric_point(
            &self.pool,
            conn.workspace_id,
            conn.id,
            "lastfm",
            "listeners",
            &format!("Last.fm listeners — {artist}"),
            listeners,
            OffsetDateTime::now_utc(),
        )
        .await?;
        record_metric_point(
            &self.pool,
            conn.workspace_id,
            conn.id,
            "lastfm",
            "playcount",
            &format!("Last.fm play count — {artist}"),
            playcount,
            OffsetDateTime::now_utc(),
        )
        .await?;
        tracing::info!(
            connection_id = %conn.id,
            artist = %artist,
            listeners,
            playcount,
            "last.fm artist stats recorded"
        );
        Ok(())
    }

    /// Deezer: fetch artist fan count from the free Deezer API.
    /// The `provider_account_id` stores the numeric Deezer artist ID.
    /// No API key is needed — the endpoint is unauthenticated.
    pub(super) async fn sync_deezer(
        &self,
        conn: &DueConnection,
    ) -> Result<(), GrowthMetricSyncError> {
        let artist_id = &conn.provider_account_id;
        let url = format!("https://api.deezer.com/artist/{artist_id}");
        let response = self.http_client.get(&url).send().await?;
        if !response.status().is_success() {
            return Err(GrowthMetricSyncError::ProviderApi(format!(
                "Deezer API returned HTTP {} for artist {artist_id}",
                response.status()
            )));
        }
        let body: serde_json::Value = response.json().await?;
        // A missing `nb_fan` is an error, not a zero — same rule as every
        // other platform. Deezer returns an `error` object for unknown IDs.
        if body.get("error").is_some() {
            return Err(GrowthMetricSyncError::ProviderApi(format!(
                "Deezer API returned an error for artist {artist_id}: {}",
                body.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown error")
            )));
        }
        let fan_count = body
            .get("nb_fan")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| {
                GrowthMetricSyncError::ProviderApi(format!(
                    "Deezer API returned no fan count for artist {artist_id}"
                ))
            })?;
        let name = body
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("Deezer artist");
        record_metric_point(
            &self.pool,
            conn.workspace_id,
            conn.id,
            "deezer",
            "fans",
            &format!("Deezer fans — {name}"),
            fan_count,
            OffsetDateTime::now_utc(),
        )
        .await?;
        tracing::info!(
            connection_id = %conn.id,
            artist_id = %artist_id,
            fans = fan_count,
            "deezer artist fan count recorded"
        );
        Ok(())
    }

    /// Discogs: fetch artist collection and wantlist counts from the Discogs
    /// API. The `provider_account_id` stores the numeric Discogs artist ID.
    /// A shared personal access token (CROWDRELAY_DISCOGS_TOKEN) is used for
    /// higher rate limits; the endpoint also works unauthenticated at a lower
    /// rate.
    pub(super) async fn sync_discogs(
        &self,
        conn: &DueConnection,
    ) -> Result<(), GrowthMetricSyncError> {
        let artist_id = &conn.provider_account_id;
        let url = format!("https://api.discogs.com/artists/{artist_id}");
        let mut request = self
            .http_client
            .get(&url)
            .header("User-Agent", "CrowdRelay/1.0 +https://crowdrelay.com");
        // The token is optional — the endpoint works without it at a lower
        // rate limit. When present, authenticate for the higher tier.
        if let Some(ref token) = self.discogs_token {
            request = request.header("Authorization", format!("Discogs token={token}"));
        }
        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(GrowthMetricSyncError::ProviderApi(format!(
                "Discogs API returned HTTP {} for artist {artist_id}",
                response.status()
            )));
        }
        let body: serde_json::Value = response.json().await?;
        // Discogs returns `stats.community.in_collection` and
        // `stats.community.in_wantlist` — how many users own or want the
        // artist's releases. Both are absolute levels.
        let stats = body.get("stats").and_then(|s| s.get("community"));
        let stat = |key: &str| -> Result<i64, GrowthMetricSyncError> {
            stats
                .and_then(|s| s.get(key))
                .and_then(serde_json::Value::as_i64)
                .ok_or_else(|| {
                    GrowthMetricSyncError::ProviderApi(format!(
                        "Discogs API returned no {key} for artist {artist_id}"
                    ))
                })
        };
        let in_collection = stat("in_collection")?;
        let in_wantlist = stat("in_wantlist")?;
        let name = body
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("Discogs artist");
        record_metric_point(
            &self.pool,
            conn.workspace_id,
            conn.id,
            "discogs",
            "in_collection",
            &format!("Discogs collection — {name}"),
            in_collection,
            OffsetDateTime::now_utc(),
        )
        .await?;
        record_metric_point(
            &self.pool,
            conn.workspace_id,
            conn.id,
            "discogs",
            "in_wantlist",
            &format!("Discogs wantlist — {name}"),
            in_wantlist,
            OffsetDateTime::now_utc(),
        )
        .await?;
        tracing::info!(
            connection_id = %conn.id,
            artist_id = %artist_id,
            in_collection,
            in_wantlist,
            "discogs artist stats recorded"
        );
        Ok(())
    }

    /// Bluesky: fetch actor follower count from the free public Bluesky API.
    /// The `provider_account_id` stores the handle (e.g. "virya.bsky.social").
    /// No API key is needed — the public AppView endpoint is unauthenticated.
    pub(super) async fn sync_bluesky(
        &self,
        conn: &DueConnection,
    ) -> Result<(), GrowthMetricSyncError> {
        let actor = &conn.provider_account_id;
        let url = format!(
            "https://public.api.bsky.app/xrpc/app.bsky.actor.getProfile?actor={}",
            urlencode(actor)
        );
        let response = self.http_client.get(&url).send().await?;
        if !response.status().is_success() {
            return Err(GrowthMetricSyncError::ProviderApi(format!(
                "Bluesky API returned HTTP {} for actor '{actor}'",
                response.status()
            )));
        }
        let body: serde_json::Value = response.json().await?;
        // A missing `followersCount` is an error, not a zero — same rule as
        // every other platform. Bluesky returns a 400 with an error body for
        // unknown handles, which is caught by the status check above.
        let followers_count = body
            .get("followersCount")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| {
                GrowthMetricSyncError::ProviderApi(format!(
                    "Bluesky API returned no follower count for actor '{actor}'"
                ))
            })?;
        let display_name = body
            .get("displayName")
            .and_then(|v| v.as_str())
            .unwrap_or(actor);
        record_metric_point(
            &self.pool,
            conn.workspace_id,
            conn.id,
            "bluesky",
            "followers",
            &format!("Bluesky followers — {display_name}"),
            followers_count,
            OffsetDateTime::now_utc(),
        )
        .await?;
        tracing::info!(
            connection_id = %conn.id,
            actor = %actor,
            followers = followers_count,
            "bluesky actor follower count recorded"
        );
        Ok(())
    }

    /// Bandcamp: scrape the artist's community page for supporter count.
    /// The `provider_account_id` stores the Bandcamp subdomain (e.g. "virya").
    /// No API key is needed — Bandcamp has no public API, so we parse the
    /// HTML community page which lists recent supporters. The count of
    /// supporter list items is the growth metric. While the band is small
    /// this is essentially the total; for larger bands it caps at the
    /// visible recent subset.
    pub(super) async fn sync_bandcamp(
        &self,
        conn: &DueConnection,
    ) -> Result<(), GrowthMetricSyncError> {
        let subdomain = &conn.provider_account_id;
        let url = format!("https://{subdomain}.bandcamp.com/community");
        let response = self.http_client.get(&url).send().await?;
        if !response.status().is_success() {
            return Err(GrowthMetricSyncError::ProviderApi(format!(
                "Bandcamp returned HTTP {} for {subdomain}",
                response.status()
            )));
        }
        let html = response.text().await?;
        // The community page renders supporters as <a ... class="supporter">
        // elements inside <ol class="supporters">. Counting these gives us
        // the number of recent supporters visible on the page. This is a
        // lower bound on the total supporter count, but it's the only public
        // signal Bandcamp exposes.
        let supporter_count = html
            .matches(r#"class="supporter""#)
            .count()
            .try_into()
            .unwrap_or(0i64);
        // A count of 0 could mean the page structure changed or the band
        // genuinely has no supporters. Unlike API-based platforms where a
        // missing count means an error, here 0 is a valid value — a new band
        // may have no supporters yet. But if the HTML doesn't contain the
        // "supporters" container at all, the page structure has changed and
        // we should error rather than record a misleading 0.
        if !html.contains(r#"class="supporters""#) && !html.contains("community-recent-supporters")
        {
            return Err(GrowthMetricSyncError::ProviderApi(format!(
                "Bandcamp community page for {subdomain} has no supporters section — \
                 the page structure may have changed"
            )));
        }
        record_metric_point(
            &self.pool,
            conn.workspace_id,
            conn.id,
            "bandcamp",
            "supporters",
            &format!("Bandcamp supporters — {subdomain}"),
            supporter_count,
            OffsetDateTime::now_utc(),
        )
        .await?;
        tracing::info!(
            connection_id = %conn.id,
            subdomain = %subdomain,
            supporters = supporter_count,
            "bandcamp supporter count recorded"
        );
        Ok(())
    }
}
