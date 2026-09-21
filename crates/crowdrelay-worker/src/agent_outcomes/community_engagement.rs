// The action payload for a Reddit community-engagement outcome.
//
// `include!`d into `agent_outcomes.rs` so it shares that module's scope.
// Split out to keep the parent inside the source-size ratchet.

impl AgentOutcomeWorker {
    /// Builds the `community.engage.request` payload for an admitted Reddit
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
        let subreddit = item
            .and_then(|i| i.get("subreddit"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let raw_link = item
            .and_then(|i| i.get("smart_link"))
            .and_then(Value::as_str)
            .unwrap_or("");
        // Create a tracked smart link for attribution so we can measure
        // which Reddit posts drive ticket sales / signups. Log errors
        // instead of silently swallowing them — a post without attribution
        // is still deliverable, but the operator should know the smart link
        // failed.
        let tracked_link = if !raw_link.is_empty() {
            match self
                .ensure_agent_smart_link(
                    tx,
                    outcome.workspace_id,
                    outcome,
                    raw_link,
                    "reddit",
                    Some(subreddit),
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
        // The creative family the engager chose lives on the producing
        // run's evidence row, keyed through the task that dispatched it.
        // Carrying it onto the engage action lets the published post's own
        // measurement teach the family posterior — the run-level row only
        // saw a workspace-wide number, published or not.
        let creative_family = creative_family_for_task(
            tx,
            outcome.workspace_id,
            outcome.task_id,
        )
        .await?;
        Ok((
            json!({
                "kind": "request_community_engagement",
                "target_id": target_id,
                "platform": "reddit",
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
