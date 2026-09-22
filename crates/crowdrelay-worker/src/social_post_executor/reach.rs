//! Publication reach + assignment execution for social posts.
//!
//! Split out of `social_post_executor.rs` so both stay inside the
//! source-size ratchet. One job with one shape: the commit that marks a
//! post `posted` also files its audience measurement and moves the
//! experiment assignment to `executed` — the same triple write telegram
//! and discord already make, so a published social post does not keep its
//! assignment `dispatched` forever nor leave the reach ledger without the
//! denominator the credit allocator divides fan outcomes by.

use crowdrelay_domain::growth_metrics::MetricPlatform;

use super::{ClaimedAction, SocialPostExecutorError, SocialPostExecutorWorker};

impl SocialPostExecutorWorker {
    /// Reach + assignment transition in the same commit as the post row.
    pub(super) async fn file_reach_and_execute_assignment(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        action: &ClaimedAction,
        platform: MetricPlatform,
        recipient_id: &str,
        published_ref: &str,
        estimated_reach: i32,
    ) -> Result<(), SocialPostExecutorError> {
        sqlx::query(
            r#"INSERT INTO viryaos_reach_events
                 (workspace_id, action_id, recipient_kind, recipient_id, channel,
                  template_id, estimated_reach, status, metadata, trace_id, causation_id)
               VALUES ($1, $2, 'platform_audience', $3, 'social_post',
                       'social-poster', $4, 'delivered',
                       jsonb_build_object('platform', $5, 'account', $3,
                                          'published_ref', $6), $7, $2)
               ON CONFLICT (action_id, recipient_id, channel) WHERE action_id IS NOT NULL
               DO NOTHING"#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(action.action_id)
        .bind(recipient_id)
        .bind(estimated_reach)
        .bind(platform.as_str())
        .bind(published_ref)
        .bind(action.trace_id)
        .execute(&mut **tx)
        .await?;

        // Monotonic: only dispatched → executed, so a retried publish cannot
        // walk the assignment backwards.
        sqlx::query(
            r#"
            UPDATE viryaos_experiment_assignments
            SET execution_status = 'executed',
                trace_id = COALESCE(trace_id, (SELECT trace_id FROM viryaos_autopilot_actions WHERE id = $2))
            WHERE workspace_id = $1
              AND action_id = $2
              AND execution_status = 'dispatched'
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(action.action_id)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// The audience the metric sync last measured for this platform —
    /// `instagram_followers`/`facebook_followers` — or `None` when nothing
    /// has measured it yet. Same contract as the telegram/discord readers: a
    /// guessed denominator is worse than the fallback constant.
    pub(super) async fn measured_audience(&self, platform: MetricPlatform) -> Option<i32> {
        let metric_key = platform.audience_metric_key()?;
        let value = sqlx::query_scalar::<_, f64>(
            r#"SELECT point.value::float8
               FROM viryaos_growth_metric_points AS point
               JOIN viryaos_growth_metric_series AS series ON series.id = point.series_id
               WHERE point.workspace_id = $1
                 AND series.platform = $2
                 AND series.metric_key = $3
               ORDER BY point.captured_at DESC
               LIMIT 1"#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(platform.as_str())
        .bind(metric_key)
        .fetch_optional(&self.pool)
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(
                error = %error,
                "could not read the measured social audience; filing the fallback reach"
            );
            None
        });
        // A measured zero is not a reach of zero — `estimated_reach` carries a
        // `>= 1` CHECK, so a real but empty page must read as unmeasured, not
        // as a constraint violation that rolls the posted transaction back
        // after the post is already live.
        value.and_then(|v| {
            let rounded = v.round();
            (rounded >= 1.0).then(|| rounded.min(f64::from(i32::MAX)) as i32)
        })
    }
}
