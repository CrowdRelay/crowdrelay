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
    /// (template_id, prompt, metadata) of the agent task that produced this
    /// outcome. Metadata carries the task's recorded evidence — what the
    /// context builder actually handed the model — which grounding checks
    /// validate against; prompt prose is a request, not evidence.
    async fn community_producing_task(
        &self,
        outcome: &ValidatedOutcome,
    ) -> Option<(String, String, Value)> {
        sqlx::query_as(
            "SELECT template_id, prompt, metadata FROM agent_service_tasks WHERE workspace_id=$1 AND id=$2",
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

/// Result of the community admission gate: the validated source row and its
/// batch key, or the refusal the caller records after dropping its write —
/// the gate returns rejections so its reads cannot pin the transaction open.
enum CommunityAdmission {
    Admitted(CommunityPostSourceRow, Option<Uuid>),
    Rejected(OutcomeRejection),
}

impl AgentOutcomeWorker {
    /// Admission gate for a community draft: the target must be a promoted,
    /// admitted community row; the draft must match its recorded language;
    /// the room must have been read; and the facts must name a live trusted
    /// content source. The row is fetched rather than existence-checked
    /// because its media fields and canonical public destination are what the
    /// action payload carries — the model writes the words; it never gets to
    /// substitute the source or URL.
    async fn admit_community_post(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        outcome: &ValidatedOutcome,
        producing_task: Option<&(String, String, Value)>,
        target_id: Uuid,
        producing_template: Option<&str>,
    ) -> Result<CommunityAdmission, AgentOutcomeError> {
        // `Some(language)` for an admitted community; its recorded language
        // is what the draft is held to below.
        let admitted = sqlx::query_scalar::<_, Option<String>>(
            r#"
            SELECT language FROM agent_outreach_targets
            WHERE workspace_id = $1
              AND id = $2
              AND target_kind = 'community'
              AND screening_verdict = 'admitted'
              AND status = 'promoted'
            "#,
        )
        .bind(outcome.workspace_id)
        .bind(target_id)
        .fetch_optional(&mut **tx)
        .await?;
        let Some(community_language) = admitted else {
            let rejection = OutcomeRejection::UnvettedCommunity { target_id };
            tracing::warn!(
                outcome_id = %outcome.id,
                target_id = %target_id,
                rejection = %rejection,
                "rejecting community post: target is not an admitted community"
            );
            return Ok(CommunityAdmission::Rejected(rejection));
        };
        // Language gate: every draft, not only the sample a batch approval
        // was given on. See `community_language`.
        let draft_text = ["title", "body"]
            .iter()
            .filter_map(|key| outcome.payload.item.as_ref()?.get(*key)?.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if let Some((expected, found)) =
            crowdrelay_domain::community_language::community_language_mismatch(
                &draft_text,
                community_language.as_deref(),
            )
        {
            let rejection = OutcomeRejection::CommunityLanguageMismatch { expected, found };
            tracing::warn!(outcome_id = %outcome.id, rejection = %rejection, "rejecting community post");
            return Ok(CommunityAdmission::Rejected(rejection));
        }

        if let Some(rejection) = self
            .room_not_read(&mut *tx, outcome, target_id, producing_template)
            .await?
        {
            tracing::warn!(outcome_id = %outcome.id, rejection = %rejection, "rejecting community post");
            return Ok(CommunityAdmission::Rejected(rejection));
        }

        // Source gate: the post must name the trusted content source its
        // facts come from — and which kinds it may name depends on which
        // worker produced the draft. The community engager is the
        // first-touch lane for fresh videos *and releases*; the repost
        // worker is narrower and may carry only the band's own synced
        // social posts. Events and stories remain outside this path.
        let source_gate = match producing_template {
            Some("community-repost") => "social_post",
            _ => "community_engager",
        };
        let source_id_raw = outcome
            .payload
            .item
            .as_ref()
            .and_then(|i| i.get("source_id"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let source_uuid = source_id_raw
            .as_deref()
            .and_then(|s| Uuid::parse_str(s).ok());
        if let Some(pinned) = producing_task
            .and_then(|(_, prompt, _)| pinned_community_uuid(prompt, "source_id"))
            && source_uuid != Some(pinned)
        {
            return Err(OutcomeRejection::UnsourcedPost {
                source_id: source_id_raw,
            }
            .into());
        }
        let source_row: Option<CommunityPostSourceRow> = match source_uuid {
            Some(source_id) => {
                sqlx::query_as::<_, CommunityPostSourceRow>(
                    r#"
                SELECT source_kind, metadata AS source_metadata,
                       metadata->>'media_url' AS media_url,
                       metadata->>'media_id' AS media_id,
                       metadata->>'media_type' AS media_type,
                       metadata->>'thumbnail_url' AS thumbnail_url,
                       CASE
                           WHEN source_kind = 'release'
                               THEN COALESCE(
                                   NULLIF(btrim(metadata->>'url'), ''),
                                   NULLIF(btrim(metadata->>'listen_url'), '')
                               )
                           ELSE metadata->>'url'
                       END AS source_url
                FROM content_sources
                WHERE workspace_id = $1
                  AND id = $2
                  AND (
                      ($3 = 'social_post' AND source_kind = 'social_post')
                      OR (
                          $3 = 'community_engager'
                          AND source_kind IN ('video', 'release')
                      )
                  )
                  AND active
                  AND expires_at > now()
                "#,
                )
                .bind(outcome.workspace_id)
                .bind(source_id)
                .bind(source_gate)
                .fetch_optional(&mut **tx)
                .await?
            }
            None => None,
        };
        let Some(source_row) = source_row else {
            let rejection = OutcomeRejection::UnsourcedPost {
                source_id: source_id_raw,
            };
            tracing::warn!(
                outcome_id = %outcome.id,
                rejection = %rejection,
                "rejecting community post: no live content source behind it"
            );
            return Ok(CommunityAdmission::Rejected(rejection));
        };
        Ok(CommunityAdmission::Admitted(source_row, source_uuid))
    }
}
