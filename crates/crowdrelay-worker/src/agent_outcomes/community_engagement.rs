// The action payload for an admitted community-engagement outcome.
//
// `include!`d into `agent_outcomes.rs` so it shares that module's scope.
// Split out to keep the parent inside the source-size ratchet.

fn pinned_community_uuid(prompt: &str, field: &str) -> Option<Uuid> {
    let prefix = format!("{field}:");
    prompt.lines().find_map(|line| {
        line.trim()
            .trim_start_matches("- ")
            .strip_prefix(&prefix)
            .and_then(|value| Uuid::parse_str(value.trim()).ok())
    })
}

/// Validated source facts, never model-produced media or policy.
#[derive(sqlx::FromRow)]
struct CommunityPostSourceRow {
    source_kind: String,
    source_metadata: Value,
    media_url: Option<String>,
    media_id: Option<String>,
    media_type: Option<String>,
    thumbnail_url: Option<String>,
    source_url: Option<String>,
}

impl AgentOutcomeWorker {
    async fn community_producing_task(
        &self,
        outcome: &ValidatedOutcome,
    ) -> Option<(String, String)> {
        sqlx::query_as(
            "SELECT template_id, prompt FROM agent_service_tasks WHERE workspace_id=$1 AND id=$2",
        )
        .bind(outcome.workspace_id)
        .bind(outcome.task_id)
        .fetch_optional(&self.pool)
        .await
        .unwrap_or_else(|error| {
            tracing::debug!(outcome_id=%outcome.id, %error,
                    "could not resolve producing task; strictest source gate applies");
            None
        })
    }

    /// Builds the `community.engage.request` payload for an admitted community
    /// target: binds a tracked smart link for attribution, attaches media
    /// from the validated source row (never the model's payload), and
    /// carries the creative family the producing run chose so the post's
    /// own measurement can teach the family posterior.
    async fn community_engagement_action(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        outcome: &ValidatedOutcome,
        target_id: Uuid,
        community_source: Option<&CommunityPostSourceRow>,
        source_id: Option<Uuid>,
    ) -> Result<(Value, &'static str), AgentOutcomeError> {
        let item = outcome.payload.item.as_ref();
        // The community's platform is the target's, never the model's
        // proposal — a Discord draft that says "reddit" lands in the wrong
        // lane, and the seed gate is built on this field being true.
        let target_platform: String = sqlx::query_scalar(
            "SELECT COALESCE(NULLIF(platform, ''), 'reddit') \
             FROM agent_outreach_targets WHERE workspace_id = $1 AND id = $2",
        )
        .bind(outcome.workspace_id)
        .bind(target_id)
        .fetch_optional(&mut **tx)
        .await?
        .unwrap_or_else(|| "reddit".to_owned());
        if let Some(source) = community_source
            && !crowdrelay_domain::video_promotion::platform_allowed(
                &source.source_kind,
                &source.source_metadata,
                &target_platform,
            )
        {
            return Err(OutcomeRejection::PlatformExcluded {
                platform: target_platform,
            }
            .into());
        }
        let subreddit = if target_platform == "reddit" {
            item.and_then(|i| i.get("subreddit"))
                .and_then(Value::as_str)
                .unwrap_or("")
        } else {
            // Off Reddit the ledger's address column carries the community's
            // display name; the model's `subreddit` field means nothing for
            // a forum thread or a Discord server.
            ""
        };
        let raw_link = item
            .and_then(|i| i.get("smart_link"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        let source_canonical_url = community_source
            .and_then(|s| s.source_url.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        // The model leaves `smart_link` empty on most drafts — it has no
        // reason to know the field exists. The source's own URL is the
        // destination the post promotes anyway, so a blank proposal falls
        // back to it: the same words go out, but through a counted `/l/`
        // redirect instead of an untracked bare link. A model proposal that
        // IS the canonical URL passes the domain check for the same reason;
        // one that points elsewhere is still refused.
        // A canonical source URL wins over a model-proposed tracked link.
        // Wrapping /l/ in another /l/ splits attribution and counts two hops.
        let destination = source_canonical_url.unwrap_or(raw_link);
        let campaign_id = match source_id {
            Some(source_id) => {
                crowdrelay_infra::promotion_campaign::ensure_source_campaign(
                    tx,
                    outcome.workspace_id,
                    source_id,
                )
                .await?
            }
            None => None,
        };
        // The creative family the engager chose lives on the producing
        // run's evidence row, keyed through the task that dispatched it.
        // Carrying it onto the engage action lets the published post's own
        // measurement teach the family posterior — the run-level row only
        // saw a workspace-wide number, published or not. It also stamps the
        // smart link's `channel_creative`, so clicks split by angle. The
        // lookup runs on the pool, outside this transaction:
        // `agent_service_tasks` is the agents service's schema and may not
        // exist here.
        let creative_family = creative_family_for_task(
            &self.pool,
            outcome.id,
            outcome.workspace_id,
            outcome.task_id,
        )
        .await;
        // Create a tracked smart link for attribution so we can measure
        // which Reddit posts drive ticket sales / signups. Log errors
        // instead of silently swallowing them — a post without attribution
        // is still deliverable, but the operator should know the smart link
        // failed.
        let tracked_link = if !destination.is_empty() {
            match self
                .ensure_agent_smart_link(
                    tx,
                    outcome.workspace_id,
                    outcome,
                    AgentSmartLinkRequest {
                        destination,
                        campaign_id,
                        channel_source: &target_platform,
                        // `channel_community` carries a CHECK (non-blank,
                        // <=120 chars); a payload string that violates it
                        // aborts the whole outcome transaction, not just the
                        // link — normalize it here rather than relying on
                        // the savepoint in `ensure_agent_smart_link`.
                        channel_community: Some(subreddit.trim())
                            .filter(|s| !s.is_empty() && s.chars().count() <= 120),
                        channel_creative: creative_family.as_deref(),
                        source_canonical_url,
                    },
                )
                .await
            {
                Ok(link) => link,
                Err(error) => {
                    tracing::warn!(
                        outcome_id = %outcome.id,
                        error = %error,
                        "failed to create agent smart link — post will go out untracked"
                    );
                    None
                }
            }
        } else {
            None
        };
        // Media comes from the source row the gate just validated — never
        // from the model's payload. For a VIDEO the postable still is
        // `thumbnail_url`; anything else carries `media_url`. `media_id`
        // lets the executor re-mint the signed CDN URL when it has expired
        // by post time.
        let (image_url, media_id, source_url) = community_source
            .map(|s| {
                let still = if s.media_type.as_deref() == Some("VIDEO") {
                    s.thumbnail_url.clone().or(s.media_url.clone())
                } else {
                    s.media_url.clone().or(s.thumbnail_url.clone())
                };
                (still, s.media_id.clone(), s.source_url.clone())
            })
            .unwrap_or_default();
        Ok((
            json!({
                "kind": "request_community_engagement",
                "target_id": target_id,
                "platform": target_platform,
                "subreddit": subreddit,
                "title": item.and_then(|i| i.get("title")).and_then(Value::as_str).unwrap_or(""),
                "body": item.and_then(|i| i.get("body")).and_then(Value::as_str).unwrap_or(""),
                "smart_link": tracked_link,
                // The validated source row's id, not the model's string —
                // this is the batch key the relay groups on.
                "source_id": source_id,
                "creative_family": creative_family,
                "image_url": image_url,
                "media_id": media_id,
                "source_url": source_url,
            }),
            "community.engage.request",
        ))
    }
}
