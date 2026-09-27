//! The platform publish arms — Instagram, Facebook, Telegram.
//!
//! Split out of `social_post_executor.rs` so the parent stays inside the
//! source-size ratchet; `join_ask.rs`, `reach.rs` and `tracked_links.rs`
//! are the same pattern. Each arm owns its provider call and its failure
//! mapping: a refusal holds the post for a person with the platform's own
//! message rather than retrying a credential error forever.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use crowdrelay_domain::growth_metrics::MetricPlatform;
use crowdrelay_domain::publish_guard::{PublishChannel, PublishContext, review_outbound_post};

use super::{
    ClaimedAction, GRAPH_API_VERSION, SocialPostExecutorError, SocialPostExecutorWorker,
    UNMEASURED_AUDIENCE_REACH,
};

impl SocialPostExecutorWorker {
    /// Publishes a drafted caption to the tenant's own Instagram account.
    ///
    /// Two calls, because Instagram publishing is two steps: build a media
    /// container from an image URL Meta fetches itself, then publish the
    /// container. There is no text-only post on Instagram, so an image is not
    /// decoration here — it is the post.
    ///
    /// **The system chooses the image, never the model.** A model naming an
    /// image URL is the same risk as a model naming a link: it can point
    /// anywhere, and what publishes under the band's name would be whatever it
    /// picked. The selector reads the tenant's own asset rows and nothing
    /// else, so an image that is not already CrowdRelay's cannot be published.
    pub(super) async fn publish_to_instagram(
        &self,
        action: &ClaimedAction,
    ) -> Result<(), SocialPostExecutorError> {
        let Some(token) = self.facebook_page_access_token.as_ref() else {
            self.hold_for_human(action.id, "no meta access token is configured")
                .await?;
            return Ok(());
        };
        let Some(account_id) = self.instagram_account_id().await? else {
            self.hold_for_human(
                action.id,
                "no connected instagram professional account to post to",
            )
            .await?;
            return Ok(());
        };
        let caption = action.text.as_deref().unwrap_or("").trim();
        if caption.is_empty() {
            self.hold_for_human(action.id, "the draft has no caption")
                .await?;
            return Ok(());
        }
        let caption = self.publish_body(action, caption);
        // A join-ask carrying `join_ask_image_url` publishes that image —
        // the tenant chose it for this feature. Anything else falls to the
        // press-asset rotation, which keeps a run of posts from repeating
        // the same picture.
        let image_url = match action.image_url.clone() {
            Some(image_url) => Some(image_url),
            None => self.next_instagram_image().await?,
        };
        let Some(image_url) = image_url else {
            // Not a failure and not a defect: the tenant has published no
            // photo the system may use. An operator can fix it by adding one,
            // which is why the reason says what is missing.
            self.hold_for_human(
                action.id,
                "no image available: add an active photo press asset to post on instagram",
            )
            .await?;
            return Ok(());
        };

        let recent = self.recent_content_hashes("instagram").await?;
        let verdict = review_outbound_post(
            &caption,
            &PublishContext {
                channel: PublishChannel::Instagram,
                approved_origins: &[self.public_origin.as_str()],
                approved_links: &[],
                recent_content_hashes: &recent,
                // Stored hashes cover the raw draft text; the reviewed
                // caption carries the appended tracked link.
                dedupe_text: action.text.as_deref(),
            },
        );
        if let Some(reason) = verdict.hold_reason() {
            tracing::info!(
                action_id = %action.action_id,
                reason = reason.as_str(),
                "instagram post held for an operator by the publish guard"
            );
            self.hold_for_human(action.id, reason.as_str()).await?;
            return Ok(());
        }

        match self
            .submit_to_instagram(&account_id, &caption, &image_url, token)
            .await
        {
            Ok(media_id) => {
                // Post + re-anchor in one commit: the measurement window
                // starts when the audience could see the post, not at
                // dispatch — a deferred draft must not be observed across
                // dead pre-exposure time.
                // Read before the transaction: the audience snapshot is an
                // input, not part of the commit's invariant, and a mid-tx
                // pool acquire is a needless second connection held open.
                let estimated_reach = self
                    .measured_audience(MetricPlatform::Instagram)
                    .await
                    .unwrap_or(UNMEASURED_AUDIENCE_REACH);
                let mut posted_tx = self.pool.begin().await?;
                sqlx::query(
                    r#"
                    UPDATE social_posts
                    SET status = 'posted',
                        platform_post_id = $3,
                        image_url = $4,
                        posted_at = now(),
                        updated_at = now(),
                        error_message = NULL
                    WHERE workspace_id = $1 AND id = $2
                    "#,
                )
                .bind(self.workspace_id.into_uuid())
                .bind(action.id)
                .bind(&media_id)
                .bind(&image_url)
                .execute(&mut *posted_tx)
                .await?;
                crowdrelay_infra::fanbase::anchor_content_measurements_to_publication(
                    &mut posted_tx,
                    self.workspace_id.into_uuid(),
                    "social_posts",
                    action.id,
                )
                .await?;
                self.file_reach_and_execute_assignment(
                    &mut posted_tx,
                    action,
                    MetricPlatform::Instagram,
                    &account_id,
                    &media_id,
                    estimated_reach,
                )
                .await?;
                posted_tx.commit().await?;
                tracing::info!(
                    action_id = %action.action_id,
                    media_id = %media_id,
                    "instagram post published"
                );
                Ok(())
            }
            Err(SocialPostExecutorError::GraphRefused(message)) => {
                tracing::warn!(
                    action_id = %action.action_id,
                    error = %message,
                    "instagram refused the post; holding it for an operator"
                );
                self.hold_for_human(action.id, &format!("instagram refused the post: {message}"))
                    .await?;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Creates the media container and publishes it. Returns the media id.
    ///
    /// A container that is created and never published is an orphan Meta
    /// cleans up on its own, so a failure between the two steps costs nothing
    /// and must not be retried into a duplicate post.
    async fn submit_to_instagram(
        &self,
        account_id: &str,
        caption: &str,
        image_url: &str,
        token: &str,
    ) -> Result<String, SocialPostExecutorError> {
        let creation_id = self
            .graph_post(
                &format!("https://graph.facebook.com/{GRAPH_API_VERSION}/{account_id}/media"),
                &[
                    ("image_url", image_url),
                    ("caption", caption),
                    ("access_token", token),
                ],
            )
            .await?;
        self.graph_post(
            &format!("https://graph.facebook.com/{GRAPH_API_VERSION}/{account_id}/media_publish"),
            &[("creation_id", &creation_id), ("access_token", token)],
        )
        .await
    }

    /// The connected Instagram Professional account's id.
    ///
    /// Instagram publishing runs against the IG user id, which is a different
    /// identifier from the Page id even though one token covers both.
    async fn instagram_account_id(&self) -> Result<Option<String>, SocialPostExecutorError> {
        let account_id: Option<String> = sqlx::query_scalar(
            r#"
            SELECT provider_account_id
            FROM fanbase_connections
            WHERE workspace_id = $1
              AND platform = 'instagram'
              AND status = 'connected'
              AND provider_account_id IS NOT NULL
            ORDER BY updated_at DESC
            LIMIT 1
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .fetch_optional(&self.pool)
        .await?
        .flatten();
        Ok(account_id)
    }

    /// The image to publish next: the tenant's own photo assets, least
    /// recently published first.
    ///
    /// Rotation rather than "the newest photo", because posting the same
    /// picture every time is what a bot looks like — and the publish guard
    /// cannot catch it, since it compares captions and the caption changes.
    /// A photo that has never been published sorts first.
    ///
    /// Only `photo` and `logo` assets, only active ones, and only from this
    /// workspace: the point of the selector is that a model cannot introduce
    /// an image, so it reads rows an operator curated and nothing else.
    async fn next_instagram_image(&self) -> Result<Option<String>, SocialPostExecutorError> {
        let image_url: Option<String> = sqlx::query_scalar(
            r#"
            SELECT asset.url
            FROM beacon_press_assets AS asset
            LEFT JOIN LATERAL (
                SELECT max(post.posted_at) AS last_published_at
                FROM social_posts AS post
                WHERE post.workspace_id = asset.workspace_id
                  AND post.platform = 'instagram'
                  AND post.status = 'posted'
                  AND post.image_url = asset.url
            ) AS use ON true
            WHERE asset.workspace_id = $1
              AND asset.active
              AND asset.asset_kind IN ('photo', 'logo')
              AND asset.url ~* '^https://'
            ORDER BY use.last_published_at ASC NULLS FIRST,
                     asset.sort_order,
                     asset.asset_key
            LIMIT 1
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(image_url)
    }

    /// Publishes a drafted post to the tenant's own Facebook Page.
    ///
    /// The Page id is the connection's `provider_account_id` — the same one
    /// `growth_metric_sync` reads Page metrics from, so publishing and
    /// measuring cannot drift onto different Pages.
    ///
    /// Every refusal path holds the draft rather than failing it: a missing
    /// token, a missing connection, a guard verdict and a Graph API refusal
    /// all leave a post a person can still publish, with the reason recorded.
    pub(super) async fn publish_to_facebook_page(
        &self,
        action: &ClaimedAction,
    ) -> Result<(), SocialPostExecutorError> {
        let Some(token) = self.facebook_page_access_token.as_ref() else {
            self.hold_for_human(action.id, "no facebook page access token is configured")
                .await?;
            return Ok(());
        };
        let Some(page_id) = self.facebook_page_id().await? else {
            self.hold_for_human(action.id, "no connected facebook page to post to")
                .await?;
            return Ok(());
        };
        let body = action.text.as_deref().unwrap_or("").trim();
        if body.is_empty() {
            self.hold_for_human(action.id, "the draft has no text")
                .await?;
            return Ok(());
        }
        // The tracked link is part of the published body — a post that
        // carries it can have its clicks counted; one that does not is
        // content that chose to be unmeasured.
        let body = self.publish_body(action, body);

        // The read a person used to do before a post went out under the
        // band's name. A held post lands in the operator queue with its
        // reason, so the worst case of automatic mode is the behaviour that
        // preceded it.
        let recent = self.recent_content_hashes("facebook").await?;
        let verdict = review_outbound_post(
            &body,
            &PublishContext {
                channel: PublishChannel::Social,
                approved_origins: &[self.public_origin.as_str()],
                approved_links: &[],
                recent_content_hashes: &recent,
                // Stored hashes cover the raw draft text; the reviewed body
                // carries the appended tracked link.
                dedupe_text: action.text.as_deref(),
            },
        );
        if let Some(reason) = verdict.hold_reason() {
            tracing::info!(
                action_id = %action.action_id,
                reason = reason.as_str(),
                "facebook post held for an operator by the publish guard"
            );
            self.hold_for_human(action.id, reason.as_str()).await?;
            return Ok(());
        }

        // An image the action carries — today that means a join-ask with
        // `join_ask_image_url` set — turns the post into a photo post: Meta
        // fetches the URL itself, the caption carries the words and the
        // tracked link. A photo ask reads like the band's own posts; a bare
        // feed entry with a link reads like an ad nobody wrote.
        let submitted = match action.image_url.as_deref() {
            Some(image_url) => {
                self.submit_to_facebook_photo(&page_id, &body, image_url, token)
                    .await
            }
            None => self.submit_to_facebook_page(&page_id, &body, token).await,
        };
        match submitted {
            Ok(post_id) => {
                // Same one-commit shape as the Instagram arm: post +
                // re-anchored measurement windows land or neither does.
                let estimated_reach = self
                    .measured_audience(MetricPlatform::Facebook)
                    .await
                    .unwrap_or(UNMEASURED_AUDIENCE_REACH);
                let mut posted_tx = self.pool.begin().await?;
                sqlx::query(
                    r#"
                    UPDATE social_posts
                    SET status = 'posted',
                        platform_post_url = $3,
                        image_url = COALESCE($4, image_url),
                        posted_at = now(),
                        updated_at = now(),
                        error_message = NULL
                    WHERE workspace_id = $1 AND id = $2
                    "#,
                )
                .bind(self.workspace_id.into_uuid())
                .bind(action.id)
                .bind(format!("https://www.facebook.com/{post_id}"))
                .bind(action.image_url.as_deref())
                .execute(&mut *posted_tx)
                .await?;
                crowdrelay_infra::fanbase::anchor_content_measurements_to_publication(
                    &mut posted_tx,
                    self.workspace_id.into_uuid(),
                    "social_posts",
                    action.id,
                )
                .await?;
                self.file_reach_and_execute_assignment(
                    &mut posted_tx,
                    action,
                    MetricPlatform::Facebook,
                    &page_id,
                    &format!("https://www.facebook.com/{post_id}"),
                    estimated_reach,
                )
                .await?;
                posted_tx.commit().await?;
                tracing::info!(
                    action_id = %action.action_id,
                    post_id = %post_id,
                    "facebook page post published"
                );
                Ok(())
            }
            // A refusal is a fact about the credential or the content, not a
            // transient failure, so it holds rather than retries. The most
            // likely one is the Page token lacking `pages_manage_posts`, and
            // retrying that forever would bury it.
            Err(SocialPostExecutorError::GraphRefused(message)) => {
                tracing::warn!(
                    action_id = %action.action_id,
                    error = %message,
                    "facebook refused the post; holding it for an operator"
                );
                self.hold_for_human(action.id, &format!("facebook refused the post: {message}"))
                    .await?;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// POSTs the message to the Page feed and returns the created post id.
    async fn submit_to_facebook_page(
        &self,
        page_id: &str,
        message: &str,
        token: &str,
    ) -> Result<String, SocialPostExecutorError> {
        self.graph_post(
            &format!("https://graph.facebook.com/{GRAPH_API_VERSION}/{page_id}/feed"),
            &[("message", message), ("access_token", token)],
        )
        .await
    }

    /// POSTs a photo to the Page and returns the created post id — the same
    /// fetch-our-URL pattern as the Instagram container: Meta reads
    /// `image_url` itself, so the image must live on an origin it can reach.
    /// The caption carries the post text and its tracked link.
    async fn submit_to_facebook_photo(
        &self,
        page_id: &str,
        caption: &str,
        image_url: &str,
        token: &str,
    ) -> Result<String, SocialPostExecutorError> {
        self.graph_post(
            &format!("https://graph.facebook.com/{GRAPH_API_VERSION}/{page_id}/photos"),
            &[
                ("url", image_url),
                ("caption", caption),
                ("access_token", token),
            ],
        )
        .await
    }

    /// Publishes the post to the tenant's Telegram channel via the Bot API.
    ///
    /// The channel and bot token come from the `telegram`
    /// `fanbase_connections` row — the same credential the
    /// `telegram-poster` executor decrypts, with the same AAD. An image the
    /// action carries goes as `sendPhoto` (Telegram fetches the URL); a
    /// text-only ask goes as `sendMessage`.
    ///
    /// `CROWDRELAY_TELEGRAM_AUTO_POST` is the worker-side kill switch,
    /// separate from `social_auto_post`: off means the post waits as
    /// `awaiting_manual_post` rather than publishing.
    pub(super) async fn publish_to_telegram(
        &self,
        action: &ClaimedAction,
    ) -> Result<(), SocialPostExecutorError> {
        if !self.telegram_auto_post {
            sqlx::query(
                r#"
                UPDATE social_posts
                SET status = 'awaiting_manual_post',
                    error_message = 'CROWDRELAY_TELEGRAM_AUTO_POST is not enabled',
                    updated_at = now()
                WHERE workspace_id = $1 AND id = $2
                "#,
            )
            .bind(self.workspace_id.into_uuid())
            .bind(action.id)
            .execute(&self.pool)
            .await?;
            return Ok(());
        }
        let Some((channel, bot_token)) = self.telegram_posting_target().await? else {
            self.hold_for_human(action.id, "no connected telegram channel to post to")
                .await?;
            return Ok(());
        };
        let body = action.text.as_deref().unwrap_or("").trim();
        if body.is_empty() {
            self.hold_for_human(action.id, "the draft has no text")
                .await?;
            return Ok(());
        }
        let body = self.publish_body(action, body);

        let recent = self.recent_content_hashes("telegram").await?;
        let verdict = review_outbound_post(
            &body,
            &PublishContext {
                channel: PublishChannel::Telegram,
                approved_origins: &[self.public_origin.as_str()],
                approved_links: &[],
                recent_content_hashes: &recent,
                dedupe_text: action.text.as_deref(),
            },
        );
        if let Some(reason) = verdict.hold_reason() {
            tracing::info!(
                action_id = %action.action_id,
                reason = reason.as_str(),
                "telegram post held for an operator by the publish guard"
            );
            self.hold_for_human(action.id, reason.as_str()).await?;
            return Ok(());
        }

        match self
            .submit_to_telegram(&channel, &body, action.image_url.as_deref(), &bot_token)
            .await
        {
            Ok(post_url) => {
                let estimated_reach = self
                    .measured_audience(MetricPlatform::Telegram)
                    .await
                    .unwrap_or(UNMEASURED_AUDIENCE_REACH);
                let mut posted_tx = self.pool.begin().await?;
                sqlx::query(
                    r#"
                    UPDATE social_posts
                    SET status = 'posted',
                        platform_post_url = $3,
                        image_url = COALESCE($4, image_url),
                        posted_at = now(),
                        updated_at = now(),
                        error_message = NULL
                    WHERE workspace_id = $1 AND id = $2
                    "#,
                )
                .bind(self.workspace_id.into_uuid())
                .bind(action.id)
                .bind(&post_url)
                .bind(action.image_url.as_deref())
                .execute(&mut *posted_tx)
                .await?;
                crowdrelay_infra::fanbase::anchor_content_measurements_to_publication(
                    &mut posted_tx,
                    self.workspace_id.into_uuid(),
                    "social_posts",
                    action.id,
                )
                .await?;
                self.file_reach_and_execute_assignment(
                    &mut posted_tx,
                    action,
                    MetricPlatform::Telegram,
                    &channel,
                    &post_url,
                    estimated_reach,
                )
                .await?;
                posted_tx.commit().await?;
                tracing::info!(
                    action_id = %action.action_id,
                    channel = %channel,
                    "telegram channel post published"
                );
                Ok(())
            }
            // A refusal is a fact about the credential or the content — the
            // bot not being a channel admin is the common one — so it holds
            // rather than retries, the same rule the Graph refusal arm runs.
            Err(SocialPostExecutorError::TelegramRefused(message)) => {
                tracing::warn!(
                    action_id = %action.action_id,
                    error = %message,
                    "telegram refused the post; holding it for an operator"
                );
                self.hold_for_human(action.id, &format!("telegram refused the post: {message}"))
                    .await?;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// The telegram connection's posting target — `(channel, bot_token)`
    /// from the `telegram` `fanbase_connections` row, decrypted with the
    /// same AAD `TelegramExecutorWorker` uses.
    async fn telegram_posting_target(
        &self,
    ) -> Result<Option<(String, String)>, SocialPostExecutorError> {
        let ws = self.workspace_id.into_uuid();
        let row: Option<(String, Option<String>)> = sqlx::query_as(
            r#"SELECT provider_account_id, encrypted_access_token
               FROM fanbase_connections
               WHERE workspace_id = $1 AND platform = 'telegram'
                 AND status NOT IN ('invalid', 'expired')
               LIMIT 1"#,
        )
        .bind(ws)
        .fetch_optional(&self.pool)
        .await?;
        let Some((channel, encrypted)) = row else {
            return Ok(None);
        };
        let Some(encrypted) = encrypted else {
            tracing::warn!("telegram connection has no bot token stored");
            return Ok(None);
        };
        let token_bytes = URL_SAFE_NO_PAD.decode(&encrypted).map_err(|_| {
            SocialPostExecutorError::TelegramRefused(
                "stored telegram bot token is not valid base64".to_owned(),
            )
        })?;
        let aad = crate::telegram_executor::telegram_bot_aad(ws, &channel);
        let token = String::from_utf8(
            crowdrelay_infra::sensitive_response::decrypt_value(
                &token_bytes,
                &self.response_encryption_key,
                &aad,
            )
            .map_err(|error| {
                SocialPostExecutorError::TelegramRefused(format!(
                    "telegram bot token decryption failed: {error}"
                ))
            })?,
        )
        .map_err(|_| {
            SocialPostExecutorError::TelegramRefused(
                "telegram bot token is not valid UTF-8".to_owned(),
            )
        })?;
        Ok(Some((channel, token)))
    }

    /// One Bot API call — `sendPhoto` when the action carries an image
    /// (Telegram fetches the URL itself), `sendMessage` otherwise. Returns
    /// the public post URL `t.me/c/{channel}/{message}` for a channel id;
    /// the numeric `-100…` id needs the `c/` form to resolve.
    async fn submit_to_telegram(
        &self,
        channel: &str,
        text: &str,
        image_url: Option<&str>,
        bot_token: &str,
    ) -> Result<String, SocialPostExecutorError> {
        #[derive(serde::Deserialize)]
        struct BotResponse {
            ok: bool,
            description: Option<String>,
            result: Option<BotMessage>,
        }
        #[derive(serde::Deserialize)]
        struct BotMessage {
            message_id: i64,
        }

        // Telegram captions cap at 1024 — variants bound at 500 plus the
        // tracked link stay well under it.
        let (endpoint, body) = match image_url {
            Some(image_url) => (
                "sendPhoto",
                serde_json::json!({
                    "chat_id": channel,
                    "photo": image_url,
                    "caption": text,
                }),
            ),
            None => (
                "sendMessage",
                serde_json::json!({
                    "chat_id": channel,
                    "text": text,
                    "disable_web_page_preview": false,
                }),
            ),
        };

        let response = self
            .http_client
            .post(format!(
                "https://api.telegram.org/bot{bot_token}/{endpoint}"
            ))
            .json(&body)
            .send()
            .await
            .map_err(SocialPostExecutorError::TelegramRequest)?;
        let parsed: BotResponse = response
            .json()
            .await
            .map_err(SocialPostExecutorError::TelegramRequest)?;
        if !parsed.ok {
            return Err(SocialPostExecutorError::TelegramRefused(
                parsed.description.unwrap_or_else(|| {
                    "the bot api returned not-ok without a description".to_owned()
                }),
            ));
        }
        let message_id = parsed
            .result
            .map(|message| message.message_id)
            .ok_or_else(|| {
                SocialPostExecutorError::TelegramRefused(
                    "the bot api returned ok but no message".to_owned(),
                )
            })?;
        // `-100…` numeric ids map to the `t.me/c/…` form; a username channel
        // keeps its name — minus the `@` the connect form accepts and stores,
        // which would otherwise land in the URL as a literal `@`.
        let public_channel = channel
            .strip_prefix("-100")
            .map(|stripped| format!("https://t.me/c/{stripped}/{message_id}"))
            .unwrap_or_else(|| {
                format!(
                    "https://t.me/{}/{message_id}",
                    channel.trim_start_matches('@')
                )
            });
        Ok(public_channel)
    }

    /// One Graph API write, returning the id it created.
    ///
    /// Shared by the Page feed and both Instagram steps so there is one place
    /// that decides what a Graph response means. A refusal carries Meta's own
    /// message: `(#200) Requires pages_manage_posts permission` is something an
    /// operator can act on and "posting failed" is not.
    ///
    /// Form body rather than query string throughout — the token is a
    /// credential, and a URL is the one part of a request proxies log.
    async fn graph_post(
        &self,
        url: &str,
        form: &[(&str, &str)],
    ) -> Result<String, SocialPostExecutorError> {
        #[derive(serde::Deserialize)]
        struct GraphResponse {
            id: Option<String>,
            error: Option<GraphError>,
        }
        #[derive(serde::Deserialize)]
        struct GraphError {
            message: String,
        }

        let response = self
            .http_client
            .post(url)
            .form(form)
            .send()
            .await
            .map_err(SocialPostExecutorError::GraphRequest)?;
        let parsed: GraphResponse = response
            .json()
            .await
            .map_err(SocialPostExecutorError::GraphRequest)?;
        if let Some(error) = parsed.error {
            return Err(SocialPostExecutorError::GraphRefused(error.message));
        }
        parsed.id.ok_or_else(|| {
            SocialPostExecutorError::GraphRefused(
                "the graph api returned neither an id nor an error".to_owned(),
            )
        })
    }

    /// The connected Facebook Page's id, if the tenant has one.
    async fn facebook_page_id(&self) -> Result<Option<String>, SocialPostExecutorError> {
        let page_id: Option<String> = sqlx::query_scalar(
            r#"
            SELECT provider_account_id
            FROM fanbase_connections
            WHERE workspace_id = $1
              AND platform = 'facebook'
              AND status = 'connected'
              AND provider_account_id IS NOT NULL
            ORDER BY updated_at DESC
            LIMIT 1
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .fetch_optional(&self.pool)
        .await?
        .flatten();
        Ok(page_id)
    }
}
