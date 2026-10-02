// The gate that keeps a community post from entering a room the band has not
// read. `include!`d into `agent_outcomes.rs` so it shares that module's scope;
// split out to keep the parent inside the source-size ratchet.
//
// The prompt gave the drafter the threads the community sweep recorded for the
// room and told it to name one (`fits_thread_url`). The prompt is a request;
// this is the check. The reading is the sweep's, not the model's: the cited URL
// must be one of the threads recorded for THIS target's room in the last
// fortnight, and the room must still count as read when the answer arrives (it
// can age out between dispatch and answer). The community repost template is
// the one exception: it carries the band's own post word for word into a room
// it was already admitted to and is handed no threads, so it has nothing to
// cite. An unknown producer is NOT the exception — a missing task row falls to
// the strict side, like the source gate beside it.

impl AgentOutcomeWorker {
    /// `Some(rejection)` when the post names no thread the band read in the
    /// target's room.
    async fn room_not_read(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        outcome: &ValidatedOutcome,
        target_id: Uuid,
        producing_template: Option<&str>,
    ) -> Result<Option<OutcomeRejection>, AgentOutcomeError> {
        if producing_template == Some("community-repost") {
            return Ok(None);
        }
        let rows: Vec<(String, String, time::Date)> = sqlx::query_as(
            r#"
            SELECT DISTINCT ON (fo.url) fo.fact, fo.url, fo.observed_at
            FROM agent_outreach_targets AS t
            JOIN fan_observations AS fo
              ON fo.workspace_id = t.workspace_id
             AND fo.place_id = t.place_id
            WHERE t.workspace_id = $1
              AND t.id = $2
              AND fo.kind = 'post'
              AND fo.url IS NOT NULL
              AND fo.observed_at >= current_date - 14
              AND fo.observed_at <= current_date
            ORDER BY fo.url, fo.observed_at DESC
            "#,
        )
        .bind(outcome.workspace_id)
        .bind(target_id)
        .fetch_all(&mut **tx)
        .await?;
        let threads: Vec<crowdrelay_domain::room_reading::RoomThread> = rows
            .into_iter()
            .map(
                |(title, url, posted_on)| crowdrelay_domain::room_reading::RoomThread {
                    title,
                    url,
                    posted_on,
                },
            )
            .collect();
        let cited = outcome
            .payload
            .item
            .as_ref()
            .and_then(|item| item.get("fits_thread_url"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let read = crowdrelay_domain::room_reading::is_read(&threads);
        let cites = cited
            .as_deref()
            .is_some_and(|url| crowdrelay_domain::room_reading::cites_a_thread(url, &threads));
        Ok((!read || !cites).then_some(OutcomeRejection::UnreadRoom { target_id, cited }))
    }
}
