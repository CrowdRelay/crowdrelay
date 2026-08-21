//! Growth delivery observability adapter.
//!
//! Reads the campaign delivery ledger and the outreach tables so operators can
//! see whether the external n8n workers are draining the work CrowdRelay
//! queued. Everything here is read-only; the growth loop's writes stay on the
//! existing internal delivery endpoints.

use super::*;

/// Row shape for the per-campaign delivery progress query.
#[derive(sqlx::FromRow)]
struct GrowthCampaignRow {
    id: Uuid,
    slug: String,
    name: String,
    template_key: String,
    status: String,
    scheduled_at: Option<OffsetDateTime>,
    completed_at: Option<OffsetDateTime>,
    recipient_count: i64,
    delivered_count: i64,
    failed_count: i64,
    claimed_count: i64,
}

impl PostgresAutopilotRepository {
    pub(super) async fn growth_overview(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<AutopilotGrowthOverview, RepositoryError> {
        let workspace = workspace_id.into_uuid();
        let templates: Vec<String> = GROWTH_TEMPLATE_KEYS
            .iter()
            .map(|key| (*key).to_owned())
            .collect();

        let campaigns_enabled = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT COALESCE((
                SELECT enabled FROM ecosystem_feature_flags
                WHERE workspace_id = $1 AND key = 'communication_campaigns_enabled'
            ), false)
            "#,
        )
        .bind(workspace)
        .fetch_one(&self.pool)
        .await
        .map_err(map_sqlx)?;

        // Counts are derived from the delivery ledger rather than the campaign
        // summary columns: the summary columns are only written when a worker
        // completes a campaign, so a stalled campaign would otherwise report
        // zeroes indistinguishable from a finished one.
        let rows = sqlx::query_as::<_, GrowthCampaignRow>(
            r#"
            SELECT
                campaign.id,
                campaign.slug,
                campaign.name,
                campaign.template_key,
                campaign.status,
                campaign.scheduled_at,
                campaign.completed_at,
                COALESCE(recipients.total, 0) AS recipient_count,
                COALESCE(deliveries.delivered, 0) AS delivered_count,
                COALESCE(deliveries.failed, 0) AS failed_count,
                COALESCE(deliveries.claimed, 0) AS claimed_count
            FROM communication_campaigns AS campaign
            LEFT JOIN LATERAL (
                SELECT count(*)::bigint AS total
                FROM communication_campaign_recipients AS recipient
                WHERE recipient.workspace_id = campaign.workspace_id
                  AND recipient.campaign_id = campaign.id
            ) AS recipients ON true
            LEFT JOIN LATERAL (
                SELECT
                    count(*) FILTER (WHERE delivery.status = 'delivered')::bigint AS delivered,
                    count(*) FILTER (WHERE delivery.status = 'failed')::bigint AS failed,
                    count(*) FILTER (WHERE delivery.status = 'claimed')::bigint AS claimed
                FROM communication_campaign_deliveries AS delivery
                WHERE delivery.workspace_id = campaign.workspace_id
                  AND delivery.campaign_id = campaign.id
            ) AS deliveries ON true
            WHERE campaign.workspace_id = $1
              AND campaign.template_key = ANY($2)
            ORDER BY campaign.scheduled_at DESC NULLS LAST, campaign.created_at DESC
            LIMIT 50
            "#,
        )
        .bind(workspace)
        .bind(&templates)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;

        let stall_cutoff = now - time::Duration::minutes(GROWTH_STALL_AFTER_MINUTES);
        let mut totals = GrowthDeliveryTotals::default();
        let campaigns: Vec<GrowthCampaignProgress> = rows
            .into_iter()
            .map(|row| {
                let resolved = row.delivered_count + row.failed_count + row.claimed_count;
                let pending = (row.recipient_count - resolved).max(0);
                // Due, has work, and no worker has claimed anything yet.
                let stalled = row.status == "scheduled"
                    && row.recipient_count > 0
                    && resolved == 0
                    && row.scheduled_at.is_some_and(|at| at <= stall_cutoff);

                match row.status.as_str() {
                    "scheduled" => totals.scheduled_campaigns += 1,
                    "completed" => totals.completed_campaigns += 1,
                    "cancelled" => totals.cancelled_campaigns += 1,
                    _ => {}
                }
                totals.delivered += row.delivered_count;
                totals.failed += row.failed_count;
                totals.claimed += row.claimed_count;
                totals.pending += pending;
                if stalled {
                    totals.stalled_campaigns += 1;
                }

                GrowthCampaignProgress {
                    campaign_id: row.id.to_string(),
                    slug: row.slug,
                    name: row.name,
                    template_key: row.template_key,
                    status: row.status,
                    scheduled_at: row.scheduled_at,
                    completed_at: row.completed_at,
                    recipient_count: row.recipient_count,
                    delivered_count: row.delivered_count,
                    failed_count: row.failed_count,
                    claimed_count: row.claimed_count,
                    pending_count: pending,
                    stalled,
                }
            })
            .collect();

        let outreach = self.growth_outreach(workspace).await?;

        Ok(AutopilotGrowthOverview {
            campaigns_enabled,
            totals,
            outreach,
            campaigns,
        })
    }

    async fn growth_outreach(
        &self,
        workspace: Uuid,
    ) -> Result<GrowthOutreachSummary, RepositoryError> {
        let (active_opportunities, playlist_opportunities) =
            sqlx::query_as::<_, (i64, i64)>(
                r#"
                SELECT
                    count(*) FILTER (WHERE opportunity.active AND opportunity.expires_at > now())::bigint,
                    count(*) FILTER (
                        WHERE opportunity.active
                          AND opportunity.expires_at > now()
                          AND opportunity.template_key = $2
                    )::bigint
                FROM viryaos_outreach_opportunities AS opportunity
                WHERE opportunity.workspace_id = $1
                "#,
            )
            .bind(workspace)
            .bind(PLAYLIST_TEMPLATE_KEY)
            .fetch_one(&self.pool)
            .await
            .map_err(map_sqlx)?;

        // The seeder's eligibility gate is mirrored here so an operator can see
        // why playlist seeding produced nothing without reading the migration.
        let (awaiting_reply, replies_14d, eligible_playlist_targets, suppressed_targets) =
            sqlx::query_as::<_, (i64, i64, i64, i64)>(
                r#"
                SELECT
                    count(*) FILTER (
                        WHERE target.last_outreach_at IS NOT NULL
                          AND target.last_reply_at IS NULL
                    )::bigint,
                    count(*) FILTER (
                        WHERE target.last_reply_at >= now() - INTERVAL '14 days'
                    )::bigint,
                    count(*) FILTER (
                        WHERE target.target_kind = 'playlist'
                          AND target.active
                          AND target.verified
                          AND target.accepts_outreach
                          AND NOT target.do_not_contact
                    )::bigint,
                    count(*) FILTER (WHERE target.do_not_contact)::bigint
                FROM viryaos_outreach_targets AS target
                WHERE target.workspace_id = $1
                "#,
            )
            .bind(workspace)
            .fetch_one(&self.pool)
            .await
            .map_err(map_sqlx)?;

        Ok(GrowthOutreachSummary {
            active_opportunities,
            playlist_opportunities,
            awaiting_reply,
            replies_14d,
            eligible_playlist_targets,
            suppressed_targets,
        })
    }
}
