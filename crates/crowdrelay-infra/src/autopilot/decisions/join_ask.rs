macro_rules! decision_join_ask_reads {
    () => {
    /// The join-ask snapshot for one cycle (§5).
    ///
    /// Five small reads, all in this workspace's scope: the tenant's own
    /// settings rows, the connected fanbase channels, the post ledger the
    /// cadence and rotation read, and the press-photo count Instagram needs.
    /// `None` when the tenant never wrote a usable variants list — the
    /// settings reader already owns that distinction.
    async fn load_join_ask_snapshot_impl(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Option<crowdrelay_domain::join_ask::JoinAskSnapshot>, RepositoryError> {
        self.bounded(async {
            use crowdrelay_domain::join_ask::{JoinAskPostRow, JoinAskSnapshot};

            let Some(config) = crate::tenant_settings::TenantSettingsRepository::new(
                self.pool.clone(),
            )
            .join_ask_config(workspace_id.into_uuid())
            .await
            .map_err(map_sqlx)?
            else {
                return Ok(None);
            };

            // The site URL and the standing publish approval ride the brand
            // seam, not a raw row read: `member_site_base_url` carries the
            // tenant's configured default, and every other consumer of the
            // setting builds links the same way. An emptied value still maps
            // to `None` — the domain's `NoSiteUrl` hold rather than a link
            // with no destination.
            let brand = crate::tenant_settings::TenantSettingsRepository::new(
                self.pool.clone(),
            )
            .brand_settings(workspace_id.into_uuid())
            .await
            .map_err(map_sqlx)?;
            let member_site_base_url = {
                let url = brand.member_site_base_url.trim().to_owned();
                (!url.is_empty()).then_some(url)
            };
            let social_auto_post = brand.social_auto_post;

            let connected_platforms = sqlx::query_scalar::<_, String>(
                r#"
                SELECT platform FROM fanbase_connections
                WHERE workspace_id = $1 AND status = 'connected'
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;

            // Only posts this feature filed count toward cadence and
            // rotation — a `social_posts` row from an agent draft is a
            // different ledger, and folding it in would let an LLM post
            // delay the tenant's own ask.
            let posts = sqlx::query_as::<_, (String, String, OffsetDateTime)>(
                r#"
                SELECT post.platform, post.status, post.created_at
                FROM social_posts AS post
                JOIN autopilot_actions AS action
                  ON action.workspace_id = post.workspace_id
                 AND action.id = post.action_id
                WHERE post.workspace_id = $1
                  AND action.action_kind = 'social.join_ask.publish'
                ORDER BY post.created_at
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?
            .into_iter()
            .map(|(platform, status, created_at)| JoinAskPostRow {
                platform,
                status,
                created_at,
            })
            .collect();

            // Instagram has no text-only post: the executor's selector reads
            // the tenant's own press assets, so the eligibility check counts
            // the same pool it would publish from.
            let instagram_photo_count = sqlx::query_scalar::<_, i64>(
                r#"
                SELECT COUNT(*) FROM beacon_press_assets
                WHERE workspace_id = $1 AND active AND asset_kind = 'photo'
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_one(&self.pool)
            .await
            .map_err(map_sqlx)?;

            Ok(Some(JoinAskSnapshot {
                variants: config.variants,
                cadence_days: config.cadence_days,
                platforms: config.platforms,
                image_url: config.image_url,
                member_site_base_url,
                social_auto_post,
                connected_platforms,
                posts,
                instagram_photo_count: u32::try_from(instagram_photo_count).unwrap_or(u32::MAX),
            }))
        })
        .await
    }
    };
}
