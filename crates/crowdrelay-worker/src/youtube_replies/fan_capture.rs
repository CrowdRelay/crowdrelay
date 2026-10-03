//! Converts a fresh owned YouTube upload into an owned-fan acquisition path.
//!
//! Distribution already sends people to the video. This companion makes the
//! source itself capable of converting that attention: one comment, in the
//! tenant's own join-ask words, points at a source-campaign smart link whose
//! destination is the first-party Signal signup. The click therefore carries
//! the same anonymous visitor/campaign provenance as every other CrowdRelay
//! smart link, and a later signup teaches source ROI about a real fan rather
//! than a view.
//!
//! The lane is deliberately conservative:
//! - it posts only under the tenant's own narrow YouTube grant
//!   (`youtube_capture_comment_auto_post`) plus the deployment gate; the broad
//!   `social_auto_post` switch for Pages and Instagram is not consulted, and
//!   without the grant it only prepares a draft for a person;
//! - it never comments when the video description already contains `/signal`;
//! - it posts at most once per source, with a small workspace daily cap;
//! - it gives up after bounded failures and leaves the reason on source metadata;
//! - it never edits the video description or invents copy.

use super::{API_BASE, Created, YoutubeRepliesWorker, flag, is_youtube_id};
use crowdrelay_infra::tenant_settings::TenantSettingsRepository;
use uuid::Uuid;

const CAPTURE_WINDOW_DAYS: i32 = 30;
const CAPTURE_COMMENTS_PER_24H: i64 = 4;
const CAPTURE_MAX_ATTEMPTS: i32 = 3;
const CAPTURE_CLAIM_TTL_SECONDS: i64 = 2 * 60 * 60;

pub(super) fn capture_slug(source_id: Uuid) -> String {
    format!("capture-youtube-{}", source_id.simple())
}

pub(super) fn signal_destination(site_root: &str, source_id: Uuid) -> String {
    format!(
        "{}/signal?utm_source=youtube&utm_medium=video_comment&utm_campaign=content_{}&utm_content=fan_capture_comment",
        site_root.trim_end_matches('/'),
        source_id.simple()
    )
}

pub(super) fn capture_comment_text(variant: &str, public_link: &str) -> String {
    format!("{}\n\n{}", variant.trim(), public_link)
}

impl YoutubeRepliesWorker {
    /// Posts at most one source-attributed acquisition comment.
    ///
    /// The source row is claimed before leaving PostgreSQL so two worker
    /// replicas do not both publish. A crashed claim expires after two hours;
    /// each claim consumes one bounded attempt, so a permanently broken video
    /// cannot become an infinite YouTube API loop.
    pub(super) async fn seed_fan_capture_comment(&self) -> Result<usize, sqlx::Error> {
        let settings = TenantSettingsRepository::new(self.pool.clone());
        let Some(config) = settings.join_ask_config(self.workspace_id).await? else {
            return Ok(0);
        };
        if config.variants.is_empty() {
            return Ok(0);
        }
        let variants = config.variants;
        let brand = settings.brand_settings(self.workspace_id).await?;
        let Some(site_root) = brand.site_root().map(str::to_owned) else {
            return Ok(0);
        };
        // Nothing may post it: prepare the placement for a person instead of
        // doing nothing. Preparing writes nothing to YouTube. Posting is the
        // tenant's own narrow YouTube grant plus the deployment gate; the broad
        // `social_auto_post` switch (Pages, Instagram feeds) is not consulted.
        if super::fan_capture_draft::capture_mode(
            flag("CROWDRELAY_SOCIAL_AUTO_POST"),
            brand.youtube_capture_comment_auto_post,
        ) == super::fan_capture_draft::CaptureMode::Prepare
        {
            return Ok(usize::from(
                super::fan_capture_draft::prepare_fan_capture_draft(
                    &self.pool,
                    self.workspace_id,
                    &variants,
                    &site_root,
                )
                .await?,
            ));
        }
        let Some(token) = self.access_token().await? else {
            return Ok(0);
        };

        let posted_24h = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*)
            FROM content_sources
            WHERE workspace_id = $1
              AND source_kind = 'video'
              AND CASE
                    WHEN COALESCE(metadata->>'fan_capture_comment_posted_unix', '')
                         ~ '^[0-9]+$'
                    THEN (metadata->>'fan_capture_comment_posted_unix')::bigint
                    ELSE 0
                  END >= EXTRACT(EPOCH FROM now())::bigint - 86400
            "#,
        )
        .bind(self.workspace_id)
        .fetch_one(&self.pool)
        .await?;
        if posted_24h >= CAPTURE_COMMENTS_PER_24H {
            return Ok(0);
        }
        // The same channel commenting under several of its own videos in quick
        // succession reads as automation to YouTube's filters, whatever the
        // words. Space the posts out; the daily cap is a ceiling, not a pace.
        let last_posted_unix = sqlx::query_scalar::<_, Option<i64>>(
            r#"
            SELECT max(CASE
                         WHEN COALESCE(metadata->>'fan_capture_comment_posted_unix', '') ~ '^[0-9]+$'
                         THEN (metadata->>'fan_capture_comment_posted_unix')::bigint
                       END)
            FROM content_sources
            WHERE workspace_id = $1 AND source_kind = 'video'
            "#,
        )
        .bind(self.workspace_id)
        .fetch_one(&self.pool)
        .await?;
        let now_unix = sqlx::query_scalar::<_, i64>("SELECT EXTRACT(EPOCH FROM now())::bigint")
            .fetch_one(&self.pool)
            .await?;
        if !super::fan_capture_draft::spaced_enough(last_posted_unix, now_unix) {
            return Ok(0);
        }

        let mut tx = self.pool.begin().await?;
        let candidate: Option<(Uuid, String, i32)> = sqlx::query_as(
            r#"
            SELECT id, source_key,
                   CASE
                     WHEN COALESCE(metadata->>'fan_capture_comment_attempts', '')
                          ~ '^[0-9]+$'
                     THEN (metadata->>'fan_capture_comment_attempts')::int
                     ELSE 0
                   END AS attempts
            FROM content_sources
            WHERE workspace_id = $1
              AND source_kind = 'video'
              AND source_key LIKE 'youtube:%'
              AND active
              AND occurred_at > now() - make_interval(days => $2)
              AND NOT (metadata ? 'fan_capture_comment_posted_unix')
              -- Confirmation loss is not failure. The provider may already
              -- have accepted the comment; never post it again blindly.
              AND NOT (metadata ? 'fan_capture_comment_unknown_at')
              -- A person was handed this comment to post; a second one from
              -- the machine would be a duplicate under the same video.
              AND NOT (metadata ? 'fan_capture_draft_at')
              -- A source that already carries the owned CTA does not need a
              -- second one in comments.
              AND COALESCE(metadata->>'body', '') NOT ILIKE '%/signal%'
              AND CASE
                    WHEN COALESCE(metadata->>'fan_capture_comment_attempts', '')
                         ~ '^[0-9]+$'
                    THEN (metadata->>'fan_capture_comment_attempts')::int
                    ELSE 0
                  END < $3
              AND CASE
                    WHEN COALESCE(metadata->>'fan_capture_comment_claimed_unix', '')
                         ~ '^[0-9]+$'
                    THEN (metadata->>'fan_capture_comment_claimed_unix')::bigint
                    ELSE 0
                  END < EXTRACT(EPOCH FROM now())::bigint - $4
            ORDER BY occurred_at DESC
            LIMIT 1
            FOR UPDATE SKIP LOCKED
            "#,
        )
        .bind(self.workspace_id)
        .bind(CAPTURE_WINDOW_DAYS)
        .bind(CAPTURE_MAX_ATTEMPTS)
        .bind(CAPTURE_CLAIM_TTL_SECONDS)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((source_id, source_key, attempts)) = candidate else {
            tx.rollback().await?;
            return Ok(0);
        };
        let Some(video_id) = source_key
            .strip_prefix("youtube:")
            .filter(|id| is_youtube_id(id))
            .map(str::to_owned)
        else {
            sqlx::query(
                r#"
                UPDATE content_sources
                SET metadata = metadata || jsonb_build_object(
                    'fan_capture_comment_attempts', $3,
                    'fan_capture_comment_hold_reason', 'invalid youtube video id'
                )
                WHERE workspace_id = $1 AND id = $2
                "#,
            )
            .bind(self.workspace_id)
            .bind(source_id)
            .bind(CAPTURE_MAX_ATTEMPTS)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(0);
        };

        let Some(campaign_id) = crowdrelay_infra::promotion_campaign::ensure_source_campaign(
            &mut tx,
            self.workspace_id,
            source_id,
        )
        .await?
        else {
            tx.rollback().await?;
            return Ok(0);
        };

        let slug = capture_slug(source_id);
        let destination = signal_destination(&site_root, source_id);
        sqlx::query(
            r#"
            INSERT INTO smart_links
                (workspace_id, slug, destination_url, campaign_id, active,
                 channel_source, channel_community, channel_creative)
            VALUES ($1, $2, $3, $4, true, 'youtube', $5, 'fan_capture_comment')
            ON CONFLICT (workspace_id, slug) DO UPDATE SET
                destination_url = EXCLUDED.destination_url,
                campaign_id = EXCLUDED.campaign_id,
                channel_source = EXCLUDED.channel_source,
                channel_community = EXCLUDED.channel_community,
                channel_creative = EXCLUDED.channel_creative,
                active = true
            "#,
        )
        .bind(self.workspace_id)
        .bind(&slug)
        .bind(&destination)
        .bind(campaign_id)
        .bind(format!("video:{video_id}"))
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r#"
            UPDATE content_sources
            SET metadata = metadata || jsonb_build_object(
                'fan_capture_comment_claimed_unix', EXTRACT(EPOCH FROM now())::bigint,
                'fan_capture_comment_attempts', $3,
                'fan_capture_link_slug', $4
            ),
                updated_at = now()
            WHERE workspace_id = $1 AND id = $2
            "#,
        )
        .bind(self.workspace_id)
        .bind(source_id)
        .bind(attempts.saturating_add(1))
        .bind(&slug)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        let public_link = format!("{}/l/{slug}", site_root.trim_end_matches('/'));
        // Each video gets one of the tenant's own variants, chosen by the video
        // and stable across retries, so identical words do not appear under
        // every upload.
        let variant =
            super::fan_capture_draft::pick_variant(&variants, source_id).unwrap_or_default();
        let text = capture_comment_text(variant, &public_link);
        let body = serde_json::json!({
            "snippet": {
                "videoId": video_id,
                "topLevelComment": {
                    "snippet": { "textOriginal": text }
                }
            }
        });
        let result = self
            .http
            .post(format!("{API_BASE}/commentThreads"))
            .query(&[("part", "snippet")])
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await;

        match result {
            Ok(response) if response.status().is_success() => {
                let comment_id = match response.json::<Created>().await {
                    Ok(created) if is_youtube_id(&created.id) => created.id,
                    Ok(_) => {
                        self.record_fan_capture_unknown(
                            source_id,
                            "YouTube accepted fan-capture comment but returned an unusable provider id",
                        )
                        .await?;
                        return Ok(0);
                    }
                    Err(error) => {
                        tracing::warn!(
                            error = %error.without_url(),
                            "fan-capture comment succeeded but provider receipt was unreadable"
                        );
                        self.record_fan_capture_unknown(
                            source_id,
                            "YouTube accepted fan-capture comment but provider receipt was unreadable",
                        )
                        .await?;
                        return Ok(0);
                    }
                };
                sqlx::query(
                    r#"
                    UPDATE content_sources
                    SET metadata =
                        (metadata - 'fan_capture_comment_claimed_unix'
                                  - 'fan_capture_comment_hold_reason'
                                  - 'fan_capture_comment_unknown_at')
                        || jsonb_build_object(
                            'fan_capture_comment_posted_at', now(),
                            'fan_capture_comment_posted_unix',
                                EXTRACT(EPOCH FROM now())::bigint,
                            'fan_capture_comment_id', $3::text,
                            'fan_capture_link_slug', $4
                        ),
                        updated_at = now()
                    WHERE workspace_id = $1 AND id = $2
                    "#,
                )
                .bind(self.workspace_id)
                .bind(source_id)
                .bind(comment_id)
                .bind(&slug)
                .execute(&self.pool)
                .await?;
                Ok(1)
            }
            Ok(response)
                if response.status().is_client_error()
                    && response.status() != reqwest::StatusCode::TOO_MANY_REQUESTS =>
            {
                let status = response.status();
                let detail = response.text().await.unwrap_or_default();
                self.record_fan_capture_failure(
                    source_id,
                    true,
                    &format!("youtube refused fan-capture comment (HTTP {status}): {detail}"),
                )
                .await?;
                Ok(0)
            }
            Ok(response) if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                // Explicit non-execution receipt: safe to release the claim and
                // try again later.
                self.record_fan_capture_failure(
                    source_id,
                    false,
                    "youtube rate limited fan-capture comment (HTTP 429)",
                )
                .await?;
                Ok(0)
            }
            Ok(response) => {
                let status = response.status();
                self.record_fan_capture_unknown(
                    source_id,
                    &format!(
                        "provider confirmation lost after fan-capture comment attempt (HTTP {status}); do not resend automatically"
                    ),
                )
                .await?;
                Ok(0)
            }
            Err(error) => {
                tracing::warn!(
                    error = %error.without_url(),
                    "fan-capture comment outcome ambiguous; automation will not resend"
                );
                self.record_fan_capture_unknown(
                    source_id,
                    "provider confirmation lost after fan-capture comment attempt; do not resend automatically",
                )
                .await?;
                Ok(0)
            }
        }
    }

    async fn record_fan_capture_unknown(
        &self,
        source_id: Uuid,
        reason: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE content_sources
            SET metadata =
                (metadata - 'fan_capture_comment_claimed_unix')
                || jsonb_build_object(
                    'fan_capture_comment_unknown_at', now(),
                    'fan_capture_comment_hold_reason', $3
                ),
                updated_at = now()
            WHERE workspace_id = $1 AND id = $2
            "#,
        )
        .bind(self.workspace_id)
        .bind(source_id)
        .bind(reason.chars().take(500).collect::<String>())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn record_fan_capture_failure(
        &self,
        source_id: Uuid,
        terminal: bool,
        reason: &str,
    ) -> Result<(), sqlx::Error> {
        let reason = reason.chars().take(500).collect::<String>();
        if terminal {
            sqlx::query(
                r#"
                UPDATE content_sources
                SET metadata =
                    (metadata - 'fan_capture_comment_claimed_unix')
                    || jsonb_build_object(
                        'fan_capture_comment_attempts', $3,
                        'fan_capture_comment_hold_reason', $4,
                        'fan_capture_comment_last_failed_at', now()
                    ),
                    updated_at = now()
                WHERE workspace_id = $1 AND id = $2
                "#,
            )
            .bind(self.workspace_id)
            .bind(source_id)
            .bind(CAPTURE_MAX_ATTEMPTS)
            .bind(reason)
            .execute(&self.pool)
            .await?;
        } else {
            sqlx::query(
                r#"
                UPDATE content_sources
                SET metadata =
                    (metadata - 'fan_capture_comment_claimed_unix')
                    || jsonb_build_object(
                        'fan_capture_comment_hold_reason', $3,
                        'fan_capture_comment_last_failed_at', now()
                    ),
                    updated_at = now()
                WHERE workspace_id = $1 AND id = $2
                "#,
            )
            .bind(self.workspace_id)
            .bind(source_id)
            .bind(reason)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_link_is_source_scoped_and_first_party() {
        let source_id = Uuid::nil();
        assert_eq!(
            capture_slug(source_id),
            "capture-youtube-00000000000000000000000000000000"
        );
        assert_eq!(
            signal_destination("https://band.example/", source_id),
            "https://band.example/signal?utm_source=youtube&utm_medium=video_comment&utm_campaign=content_00000000000000000000000000000000&utm_content=fan_capture_comment"
        );
    }

    #[test]
    fn capture_comment_keeps_the_tenants_words_and_appends_only_the_link() {
        assert_eq!(
            capture_comment_text("  Join our Signal.  ", "https://band.example/l/capture"),
            "Join our Signal.\n\nhttps://band.example/l/capture"
        );
    }
}
