//! Label portfolio persistence: organizations, consent edges between roster
//! workspaces, and the guarded amplification delivery engine.
//!
//! Everything here runs under one rule the whole feature stands on: **fans
//! never leave home**. An amplification campaign reads the owner workspace's
//! active fans and enqueues deliveries through that same workspace's outbox;
//! the beneficiary never receives addresses, only reach numbers.

use sqlx::PgPool;
use uuid::Uuid;

use crowdrelay_domain::portfolio::{self, ConsentStatus};

#[derive(Clone)]
pub struct PostgresPortfolioRepository {
    pool: PgPool,
}

#[derive(Debug, thiserror::Error)]
pub enum PortfolioError {
    #[error("resource not found")]
    NotFound,
    /// The requested lifecycle move does not exist for the edge's state.
    #[error("decision does not apply to the current state")]
    InvalidDecision,
    /// The monthly campaign cap for this edge is already spent.
    #[error("monthly campaign cap reached for this edge")]
    CapReached,
    /// Both workspaces must belong to the same organization.
    #[error("workspaces are not in the same organization")]
    NotInSameOrganization,
    #[error("label portfolio repository failed unexpectedly")]
    Database(sqlx::Error),
}

impl PortfolioError {
    fn unexpected(error: sqlx::Error) -> Self {
        tracing::error!(error = %error, "label portfolio persistence failed");
        PortfolioError::Database(error)
    }
}

#[derive(Debug, sqlx::FromRow)]
pub struct ConsentRow {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub from_workspace_id: Uuid,
    pub to_workspace_id: Uuid,
    pub purpose: String,
    pub scope: String,
    pub status: String,
    pub max_campaigns_per_month: i16,
    pub cooldown_days: i16,
    pub approved_by: Option<String>,
    pub approved_at: Option<time::OffsetDateTime>,
    pub revoked_at: Option<time::OffsetDateTime>,
    pub revoke_reason: Option<String>,
    pub campaigns_this_month: i64,
}

#[derive(Debug, sqlx::FromRow)]
pub struct OrgOverviewRow {
    pub workspace_count: i64,
    pub active_fans: i64,
    pub fans_last_30d: i64,
    pub active_edges: i64,
    pub deliveries_last_30d: i64,
}

const CONSENT_SELECT_BASE: &str = r#"
    SELECT consent.id, consent.organization_id,
           consent.from_workspace_id, consent.to_workspace_id,
           consent.purpose, consent.scope, consent.status,
           consent.max_campaigns_per_month, consent.cooldown_days,
           consent.approved_by, consent.approved_at,
           consent.revoked_at, consent.revoke_reason,
           (SELECT count(DISTINCT ledger.campaign_reference)::bigint
            FROM amplification_deliveries AS ledger
            WHERE ledger.consent_id = consent.id
              AND ledger.delivered_at >= date_trunc('month', now()))
               AS campaigns_this_month
    FROM amplification_consents AS consent
"#;

impl PostgresPortfolioRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Creates the label organization for the calling workspace and attaches
    /// that workspace to it. Idempotent per slug; a workspace already attached
    /// elsewhere is refused rather than silently re-homed.
    pub async fn create_organization_for_workspace(
        &self,
        workspace_id: Uuid,
        slug: &str,
        name: &str,
    ) -> Result<Uuid, PortfolioError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(PortfolioError::unexpected)?;
        let attached: Option<Uuid> = sqlx::query_scalar(
            "SELECT organization_id FROM workspaces WHERE id = $1 AND organization_id IS NOT NULL",
        )
        .bind(workspace_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(PortfolioError::unexpected)?;
        if let Some(existing) = attached {
            // One portfolio per workspace: re-running setup is a no-op.
            return Ok(existing);
        }
        let org_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO organizations (slug, name) VALUES ($1, $2)
            ON CONFLICT (slug) DO UPDATE SET name = EXCLUDED.name
            RETURNING id
            "#,
        )
        .bind(slug)
        .bind(name)
        .fetch_one(&mut *tx)
        .await
        .map_err(PortfolioError::unexpected)?;
        let updated = sqlx::query(
            "UPDATE workspaces SET organization_id = $2 WHERE id = $1 AND organization_id IS NULL",
        )
        .bind(workspace_id)
        .bind(org_id)
        .execute(&mut *tx)
        .await
        .map_err(PortfolioError::unexpected)?;
        if updated.rows_affected() == 0 {
            return Err(PortfolioError::NotInSameOrganization);
        }
        tx.commit().await.map_err(PortfolioError::unexpected)?;
        Ok(org_id)
    }

    /// Proposes an amplification edge between two workspaces of one
    /// organization. The proposing side is the audience owner.
    #[allow(clippy::too_many_arguments)]
    pub async fn propose_amplification(
        &self,
        from_workspace_id: Uuid,
        to_workspace_id: Uuid,
        purpose: crowdrelay_domain::portfolio::AmplificationPurpose,
        scope: &str,
        max_campaigns_per_month: i16,
        cooldown_days: i16,
    ) -> Result<Uuid, PortfolioError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(PortfolioError::unexpected)?;
        let shared: Option<Uuid> = sqlx::query_scalar(
            r#"
            SELECT a.organization_id
            FROM workspaces AS a
            JOIN workspaces AS b ON b.organization_id = a.organization_id
            WHERE a.id = $1 AND b.id = $2 AND a.organization_id IS NOT NULL
            "#,
        )
        .bind(from_workspace_id)
        .bind(to_workspace_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(PortfolioError::unexpected)?;
        let Some(organization_id) = shared else {
            return Err(PortfolioError::NotInSameOrganization);
        };
        let id = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO amplification_consents (
                organization_id, from_workspace_id, to_workspace_id,
                purpose, scope, max_campaigns_per_month, cooldown_days
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            ON CONFLICT (from_workspace_id, to_workspace_id, purpose) DO NOTHING
            RETURNING id
            "#,
        )
        .bind(organization_id)
        .bind(from_workspace_id)
        .bind(to_workspace_id)
        .bind(purpose.as_str())
        .bind(scope)
        .bind(max_campaigns_per_month)
        .bind(cooldown_days)
        .fetch_optional(&mut *tx)
        .await
        .map_err(PortfolioError::unexpected)?
        .ok_or(PortfolioError::InvalidDecision)?;
        tx.commit().await.map_err(PortfolioError::unexpected)?;
        Ok(id)
    }

    /// Applies one operator decision. Domain policy owns which moves exist;
    /// the UPDATE's status guard decides who gets there first.
    pub async fn decide_amplification(
        &self,
        workspace_id: Uuid,
        consent_id: Uuid,
        action_target: ConsentStatus,
        approved_by: Option<&str>,
        revoke_reason: Option<&str>,
    ) -> Result<(), PortfolioError> {
        let current = self
            .consent_status(workspace_id, consent_id)
            .await?
            .ok_or(PortfolioError::NotFound)?;
        if !portfolio::can_decide(current, action_target) {
            return Err(PortfolioError::InvalidDecision);
        }
        let result = sqlx::query(
            r#"
            UPDATE amplification_consents AS consent
            SET status = $3,
                approved_by = COALESCE($5, consent.approved_by),
                approved_at = CASE WHEN $3 = 'active' THEN now() ELSE consent.approved_at END,
                revoked_at = CASE WHEN $3 = 'revoked' THEN now() ELSE NULL END,
                revoke_reason = CASE WHEN $3 = 'revoked' THEN $6 ELSE NULL END,
                updated_at = now()
            WHERE consent.id = $1
              AND consent.status = $2
              AND (consent.from_workspace_id = $4 OR consent.to_workspace_id = $4)
            "#,
        )
        .bind(consent_id)
        .bind(current.as_str())
        .bind(action_target.as_str())
        .bind(workspace_id)
        .bind(approved_by)
        .bind(revoke_reason)
        .execute(&self.pool)
        .await
        .map_err(PortfolioError::unexpected)?;
        if result.rows_affected() == 0 {
            return Err(PortfolioError::InvalidDecision);
        }
        Ok(())
    }

    pub async fn list_consents(
        &self,
        workspace_id: Uuid,
    ) -> Result<Vec<ConsentRow>, PortfolioError> {
        sqlx::query_as::<_, ConsentRow>(&format!(
            "{CONSENT_SELECT_BASE} \
             WHERE consent.from_workspace_id = $1 OR consent.to_workspace_id = $1 \
             ORDER BY consent.updated_at DESC, consent.id"
        ))
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await
        .map_err(PortfolioError::unexpected)
    }

    /// Reach preview without exposing a single address.
    pub async fn preview_audience(
        &self,
        workspace_id: Uuid,
        consent_id: Uuid,
    ) -> Result<i64, PortfolioError> {
        sqlx::query_scalar::<_, i64>(
            r#"
            WITH edge AS (
                SELECT consent.* FROM amplification_consents AS consent
                WHERE consent.id = $2
                  AND (consent.from_workspace_id = $1 OR consent.to_workspace_id = $1)
            )
            SELECT count(fan.id)::bigint
            FROM edge
            JOIN fans AS fan
              ON fan.workspace_id = edge.from_workspace_id
             AND fan.status = 'active'
            "#,
        )
        .bind(workspace_id)
        .bind(consent_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(PortfolioError::unexpected)?
        .ok_or(PortfolioError::NotFound)
    }

    /// Runs one amplification campaign through the edge: picks the owner's
    /// active fans, enqueues messages into the OWNER's outbox about the
    /// beneficiary, writes the ledger, and enforces the monthly campaign cap
    /// plus per-fan cooldown — all in one statement set inside one transaction.
    ///
    /// Returns the number of fans reached this call.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_amplification_campaign(
        &self,
        workspace_id: Uuid,
        consent_id: Uuid,
        campaign_reference: &str,
        message_subject: &str,
        message_text: &str,
        batch_limit: i64,
    ) -> Result<i64, PortfolioError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(PortfolioError::unexpected)?;
        // Explicit cap probe so a full month reads as CapReached instead of a
        // silently empty batch.
        let spent_campaigns: i32 = sqlx::query_scalar(
            r#"
            SELECT count(DISTINCT ledger.campaign_reference)::int
            FROM amplification_consents AS consent
            LEFT JOIN amplification_deliveries AS ledger
              ON ledger.consent_id = consent.id
             AND ledger.delivered_at >= date_trunc('month', now())
            WHERE consent.id = $2
              AND consent.status = 'active'
              AND (consent.from_workspace_id = $1 OR consent.to_workspace_id = $1)
            "#,
        )
        .bind(workspace_id)
        .bind(consent_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(PortfolioError::unexpected)?
        .ok_or(PortfolioError::NotFound)?;
        let max_campaigns: i16 = sqlx::query_scalar(
            "SELECT max_campaigns_per_month FROM amplification_consents WHERE id = $1",
        )
        .bind(consent_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(PortfolioError::unexpected)?;
        if spent_campaigns >= i32::from(max_campaigns) {
            return Err(PortfolioError::CapReached);
        }
        let inserted = sqlx::query_scalar::<_, i64>(
            r#"
            WITH edge AS (
                SELECT consent.*
                FROM amplification_consents AS consent
                WHERE consent.id = $2
                  AND consent.status = 'active'
                  AND (consent.from_workspace_id = $1 OR consent.to_workspace_id = $1)
            ), spent AS (
                SELECT count(DISTINCT ledger.campaign_reference)::int AS campaigns
                FROM amplification_deliveries AS ledger
                CROSS JOIN edge
                WHERE ledger.consent_id = edge.id
                  AND ledger.delivered_at >= date_trunc('month', now())
            ), audience AS (
                SELECT fan.id, fan.normalized_email, fan.display_name, fan.locale
                FROM edge
                JOIN fans AS fan
                  ON fan.workspace_id = edge.from_workspace_id
                 AND fan.status = 'active'
                WHERE NOT EXISTS (
                    SELECT 1 FROM amplification_deliveries AS prior
                    WHERE prior.consent_id = edge.id
                      AND prior.fan_id = fan.id
                      AND prior.campaign_reference = $3
                )
                AND NOT EXISTS (
                    SELECT 1 FROM amplification_deliveries AS prior
                    WHERE prior.consent_id = edge.id
                      AND prior.fan_id = fan.id
                      AND prior.delivered_at
                          > now() - make_interval(days => edge.cooldown_days::int)
                )
                ORDER BY fan.created_at, fan.id
                LIMIT $5
            ), queued AS (
                INSERT INTO outbox_events (
                    workspace_id, event_type, event_version, payload, request_id
                )
                SELECT edge.from_workspace_id,
                       'amplification.campaign_due',
                       1,
                       jsonb_build_object(
                           'consent_id', edge.id,
                           'beneficiary_workspace_id', edge.to_workspace_id,
                           'campaign_reference', $3,
                           'subject', $4,
                           'text', $6,
                           'fan', jsonb_build_object(
                               'id', audience.id,
                               'email', audience.normalized_email,
                               'display_name', audience.display_name,
                               'locale', audience.locale
                           )
                       ),
                       'amplify:' || edge.id::text || ':' || $3 || ':fan:' || audience.id::text
                FROM edge, spent, audience
                WHERE spent.campaigns < edge.max_campaigns_per_month
                RETURNING 1
            ), ledgered AS (
                INSERT INTO amplification_deliveries (
                    consent_id, from_workspace_id, to_workspace_id,
                    fan_id, campaign_reference
                )
                SELECT edge.id, edge.from_workspace_id, edge.to_workspace_id,
                       audience.id, $3
                FROM edge, spent, audience
                WHERE spent.campaigns < edge.max_campaigns_per_month
                RETURNING 1
            )
            SELECT count(*)::bigint FROM ledgered
            "#,
        )
        .bind(workspace_id)
        .bind(consent_id)
        .bind(campaign_reference)
        .bind(message_subject)
        .bind(batch_limit)
        .bind(message_text)
        .fetch_one(&mut *tx)
        .await
        .map_err(PortfolioError::unexpected)?;
        tx.commit().await.map_err(PortfolioError::unexpected)?;
        Ok(inserted)
    }

    pub async fn org_overview(&self, workspace_id: Uuid) -> Result<OrgOverviewRow, PortfolioError> {
        sqlx::query_as::<_, OrgOverviewRow>(
            r#"
            WITH scope AS (
                SELECT organization_id FROM workspaces WHERE id = $1
            ), roster AS (
                SELECT workspace.id
                FROM workspaces AS workspace
                JOIN scope ON scope.organization_id IS NOT NULL
                           AND workspace.organization_id = scope.organization_id
            ), edges AS (
                SELECT consent.* FROM amplification_consents AS consent
                WHERE consent.from_workspace_id IN (SELECT id FROM roster)
                   OR consent.to_workspace_id IN (SELECT id FROM roster)
            )
            SELECT
                (SELECT count(*)::bigint FROM roster) AS workspace_count,
                COALESCE((SELECT sum(count_by_ws.active)::bigint FROM (
                    SELECT count(*) AS active FROM fans
                    WHERE status = 'active' AND workspace_id IN (SELECT id FROM roster)
                    GROUP BY workspace_id
                ) AS count_by_ws), 0) AS active_fans,
                (SELECT count(*)::bigint FROM fans
                 WHERE status = 'active'
                   AND created_at >= now() - interval '30 days'
                   AND workspace_id IN (SELECT id FROM roster)) AS fans_last_30d,
                (SELECT count(*)::bigint FROM edges WHERE status = 'active') AS active_edges,
                (SELECT count(*)::bigint FROM amplification_deliveries AS ledger
                 WHERE ledger.delivered_at >= now() - interval '30 days'
                   AND ledger.consent_id IN (SELECT id FROM edges)) AS deliveries_last_30d
            "#,
        )
        .bind(workspace_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(PortfolioError::unexpected)?
        .ok_or(PortfolioError::NotFound)
    }

    async fn consent_status(
        &self,
        workspace_id: Uuid,
        consent_id: Uuid,
    ) -> Result<Option<ConsentStatus>, PortfolioError> {
        let raw = sqlx::query_scalar::<_, String>(
            r#"
            SELECT status FROM amplification_consents
            WHERE id = $2 AND (from_workspace_id = $1 OR to_workspace_id = $1)
            "#,
        )
        .bind(workspace_id)
        .bind(consent_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(PortfolioError::unexpected)?;
        Ok(raw.and_then(|value| ConsentStatus::from_storage(&value)))
    }
}
