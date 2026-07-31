//! PostgreSQL acquisition repository: smart-link resolution, click batching,
//! fan signup, and city signal listing.

use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use crowdrelay_application::{AcquisitionRepository, RepositoryError, SignupFanCommand};
use crowdrelay_domain::{
    CampaignId, CityId, CitySignal, CitySlug, ClickEvent, CountryCode, DestinationUrl,
    FanActionToken, FanId, FanSignup, FanSignupResult, FanStatus, ReferralCode, ResolvedSmartLink,
    SmartLinkId, SmartLinkSlug, WorkspaceId, WorkspaceSlug,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use tokio::{
    sync::{mpsc, watch},
    time::{MissedTickBehavior, interval, timeout},
};
use uuid::Uuid;

use crate::{
    config::DatabaseConfig,
    database::{SqlxErrorClass, classify_sqlx_error},
    fan_lifecycle::{issue_confirmation_token, issue_fan_action_token},
    referrals::{
        issue_fan_session, qualify_signup_referral_and_rewards, record_pending_signup_referral,
    },
    sensitive_response::SensitiveResponseCodec,
};

const IDEMPOTENCY_SCOPE: &str = "fan_signup";
const JSON_CONTENT_TYPE: &str = "application/json";
const ENCRYPTED_JSON_CONTENT_TYPE: &str = "application/vnd.crowdrelay.encrypted+json";
const IDEMPOTENCY_RETENTION_MILLISECONDS: i64 = 86_400_000;
const MAX_CLICK_BATCH_ROWS: usize = 1_000;
const MAX_CITY_SIGNAL_ROWS: u32 = 1_000;
const CONFIRMATION_RESEND_COOLDOWN_MINUTES: i64 = 15;

/// Tenant-scoped PostgreSQL implementation of the acquisition repository.
///
/// The workspace slug comes from trusted process configuration, never from a
/// public request. Repository queries additionally verify any workspace ID in
/// a command against that configured workspace.
#[derive(Clone, Debug)]
pub struct PostgresAcquisitionRepository {
    pool: PgPool,
    workspace_slug: WorkspaceSlug,
    default_country_code: CountryCode,
    operation_timeout: Duration,
    lock_timeout: Duration,
    require_double_opt_in: bool,
    sensitive_response_codec: SensitiveResponseCodec,
}

struct FanActiveOutboxArgs<'a> {
    workspace_id: WorkspaceId,
    command: &'a SignupFanCommand,
    fan_id: FanId,
    referral_code: &'a ReferralCode,
    unsubscribe_token: &'a FanActionToken,
    created: bool,
}

impl PostgresAcquisitionRepository {
    #[must_use]
    pub fn new(
        pool: PgPool,
        workspace_slug: WorkspaceSlug,
        default_country_code: CountryCode,
        database: &DatabaseConfig,
        require_double_opt_in: bool,
        sensitive_response_codec: SensitiveResponseCodec,
    ) -> Self {
        Self {
            pool,
            workspace_slug,
            default_country_code,
            operation_timeout: database.operation_timeout,
            lock_timeout: database.lock_timeout,
            require_double_opt_in,
            sensitive_response_codec,
        }
    }

    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    async fn bounded<T>(
        &self,
        operation: impl Future<Output = Result<T, StoreError>>,
    ) -> Result<T, StoreError> {
        timeout(self.operation_timeout, operation)
            .await
            .map_err(|_| StoreError::Unavailable)?
    }

    async fn trusted_workspace_id_inner(&self) -> Result<WorkspaceId, StoreError> {
        let id = sqlx::query_scalar::<_, Uuid>("SELECT id FROM workspaces WHERE slug = $1")
            .bind(self.workspace_slug.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(StoreError::from_sqlx)?
            .ok_or(StoreError::NotFound)?;

        Ok(WorkspaceId::from_uuid(id))
    }

    async fn resolve_workspace_inner(
        &self,
        slug: &WorkspaceSlug,
    ) -> Result<Option<WorkspaceId>, StoreError> {
        if slug != &self.workspace_slug {
            return Ok(None);
        }

        let id = sqlx::query_scalar::<_, Uuid>("SELECT id FROM workspaces WHERE slug = $1")
            .bind(slug.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(StoreError::from_sqlx)?;

        Ok(id.map(WorkspaceId::from_uuid))
    }

    async fn load_active_smart_links_inner(&self) -> Result<Vec<ResolvedSmartLink>, StoreError> {
        let rows = sqlx::query_as::<_, SmartLinkRow>(
            r#"
            SELECT
                smart_links.id,
                smart_links.workspace_id,
                smart_links.campaign_id,
                smart_links.slug,
                smart_links.destination_url,
                smart_links.version
            FROM smart_links
            INNER JOIN workspaces
                ON workspaces.id = smart_links.workspace_id
            LEFT JOIN campaigns
                ON campaigns.workspace_id = smart_links.workspace_id
                AND campaigns.id = smart_links.campaign_id
            WHERE workspaces.slug = $1
                AND smart_links.active
                AND (
                    smart_links.campaign_id IS NULL
                    OR campaigns.active
                )
            ORDER BY smart_links.slug, smart_links.id
            "#,
        )
        .bind(self.workspace_slug.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::from_sqlx)?;

        rows.into_iter()
            .map(|row| ResolvedSmartLink::try_from(row).map_err(|_| StoreError::Unexpected))
            .collect()
    }

    async fn persist_click_batch_inner(&self, clicks: &[ClickEvent]) -> Result<(), StoreError> {
        if clicks.is_empty() {
            return Ok(());
        }
        if clicks.len() > MAX_CLICK_BATCH_ROWS {
            return Err(StoreError::Conflict);
        }

        let workspace_id = self.trusted_workspace_id_inner().await?;
        if clicks
            .iter()
            .any(|click| click.workspace_id() != workspace_id)
        {
            return Err(StoreError::NotFound);
        }

        let workspace_ids = vec![workspace_id.into_uuid(); clicks.len()];
        let smart_link_ids: Vec<Uuid> = clicks
            .iter()
            .map(|click| click.smart_link_id().into_uuid())
            .collect();
        let campaign_ids: Vec<Option<Uuid>> = clicks
            .iter()
            .map(|click| click.campaign_id().map(Into::into))
            .collect();
        let visitor_ids: Vec<Option<Uuid>> = clicks
            .iter()
            .map(|click| click.visitor_id().map(Into::into))
            .collect();
        let referrer_hosts: Vec<Option<String>> = clicks
            .iter()
            .map(|click| click.referrer_host().map(str::to_owned))
            .collect();
        let occurred_at: Vec<OffsetDateTime> = clicks.iter().map(ClickEvent::occurred_at).collect();

        let result = sqlx::query(
            r#"
            WITH candidates (
                workspace_id,
                smart_link_id,
                campaign_id,
                anonymous_visitor_id,
                referrer_host,
                occurred_at
            ) AS (
                SELECT *
                FROM UNNEST(
                    $1::uuid[],
                    $2::uuid[],
                    $3::uuid[],
                    $4::uuid[],
                    $5::text[],
                    $6::timestamptz[]
                )
            ),
            valid_candidates AS (
                SELECT candidates.*
                FROM candidates
                INNER JOIN smart_links
                    ON smart_links.workspace_id = candidates.workspace_id
                    AND smart_links.id = candidates.smart_link_id
                    AND candidates.campaign_id
                        IS NOT DISTINCT FROM smart_links.campaign_id
            )
            INSERT INTO click_events (
                workspace_id,
                smart_link_id,
                campaign_id,
                anonymous_visitor_id,
                referrer_host,
                occurred_at
            )
            SELECT valid_candidates.*
            FROM valid_candidates
            WHERE
                (SELECT count(*) FROM valid_candidates)
                = (SELECT count(*) FROM candidates)
            "#,
        )
        .bind(&workspace_ids)
        .bind(&smart_link_ids)
        .bind(&campaign_ids)
        .bind(&visitor_ids)
        .bind(&referrer_hosts)
        .bind(&occurred_at)
        .execute(&self.pool)
        .await
        .map_err(StoreError::from_sqlx)?;

        if result.rows_affected() != u64::try_from(clicks.len()).unwrap_or(u64::MAX) {
            return Err(StoreError::Conflict);
        }

        Ok(())
    }

    async fn list_city_signals_inner(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
    ) -> Result<Vec<CitySignal>, StoreError> {
        if !(1..=MAX_CITY_SIGNAL_ROWS).contains(&limit) {
            return Err(StoreError::Conflict);
        }
        if self.trusted_workspace_id_inner().await? != workspace_id {
            return Err(StoreError::NotFound);
        }

        let rows = sqlx::query_as::<_, CitySignalRow>(
            r#"
            SELECT
                cities.id AS city_id,
                cities.slug,
                cities.name,
                cities.country_code::text AS country_code,
                city_aggregates.confirmed_fan_count AS fan_count
            FROM city_aggregates
            INNER JOIN cities
                ON cities.id = city_aggregates.city_id
            WHERE city_aggregates.workspace_id = $1
                AND cities.country_code = $2
            ORDER BY
                city_aggregates.confirmed_fan_count DESC,
                cities.name,
                cities.id
            LIMIT $3
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(self.default_country_code.as_str())
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::from_sqlx)?;

        rows.into_iter()
            .map(|row| CitySignal::try_from(row).map_err(|_| StoreError::Unexpected))
            .collect()
    }

    async fn persist_fan_signup_inner(
        &self,
        command: &SignupFanCommand,
    ) -> Result<FanSignupResult, StoreError> {
        let signup = command.signup();
        signup.validate().map_err(|_| StoreError::Conflict)?;
        if !signup.consent().granted() {
            return Err(StoreError::Conflict);
        }

        let request_bytes = serde_json::to_vec(signup).map_err(|_| StoreError::Unexpected)?;
        let request_hash = Sha256::digest(request_bytes).to_vec();
        let mut transaction = self.pool.begin().await.map_err(StoreError::from_sqlx)?;
        self.configure_transaction(&mut transaction).await?;

        let workspace_id = self
            .trusted_workspace_id_in_transaction(&mut transaction)
            .await?;
        if signup.workspace_id() != workspace_id {
            return Err(StoreError::NotFound);
        }

        let inserted_idempotency = self
            .start_idempotency(&mut transaction, workspace_id, command, &request_hash)
            .await?;
        let idempotency = self
            .lock_idempotency(&mut transaction, workspace_id, command)
            .await?;
        if idempotency.request_hash != request_hash {
            return Err(StoreError::Conflict);
        }
        if idempotency.state == "completed" {
            let response = idempotency.response_body.ok_or(StoreError::Unexpected)?;
            let result: FanSignupResult = match idempotency.response_content_type.as_deref() {
                Some(ENCRYPTED_JSON_CONTENT_TYPE) => self
                    .sensitive_response_codec
                    .decrypt(
                        workspace_id,
                        IDEMPOTENCY_SCOPE,
                        command.idempotency_key().as_str(),
                        response,
                    )
                    .map_err(|_| StoreError::Unexpected)?,
                Some(JSON_CONTENT_TYPE) | None => {
                    serde_json::from_value(response).map_err(|_| StoreError::Unexpected)?
                }
                Some(_) => return Err(StoreError::Unexpected),
            };
            self.refresh_completed_idempotency_response(
                &mut transaction,
                workspace_id,
                command,
                &request_hash,
                &result,
            )
            .await?;
            transaction.commit().await.map_err(StoreError::from_sqlx)?;
            return Ok(result);
        }
        if idempotency.state != "in_progress" {
            return Err(StoreError::Unexpected);
        }
        if !inserted_idempotency {
            if !idempotency.lease_expired {
                return Err(StoreError::Conflict);
            }
            self.reclaim_idempotency(&mut transaction, workspace_id, command)
                .await?;
        }

        let fan_upsert = self
            .upsert_fan(&mut transaction, workspace_id, signup)
            .await?;
        if fan_upsert.already_active {
            let resend_is_too_soon = self
                .fan_action_resend_is_in_cooldown(
                    &mut transaction,
                    workspace_id,
                    fan_upsert.fan.id,
                    "session",
                )
                .await?;
            if !resend_is_too_soon {
                let recovery_token = issue_fan_action_token(
                    &mut transaction,
                    workspace_id,
                    fan_upsert.fan.id,
                    "session",
                    2,
                )
                .await
                .map_err(map_lifecycle_error)?;
                self.append_session_requested_outbox(
                    &mut transaction,
                    workspace_id,
                    command,
                    fan_upsert.fan.id,
                    &recovery_token,
                )
                .await?;
            }
            let result = FanSignupResult {
                fan_id: fan_upsert.fan.id,
                status: FanStatus::Active,
                referral_code: None,
                fan_session_token: None,
                confirmation_required: true,
                created: false,
            };
            self.complete_idempotency(
                &mut transaction,
                workspace_id,
                command,
                &request_hash,
                &result,
            )
            .await?;
            transaction.commit().await.map_err(StoreError::from_sqlx)?;
            return Ok(result);
        }
        if fan_upsert.already_pending {
            let resend_is_too_soon = self
                .fan_action_resend_is_in_cooldown(
                    &mut transaction,
                    workspace_id,
                    fan_upsert.fan.id,
                    "confirm",
                )
                .await?;
            if !resend_is_too_soon {
                let confirmation_token =
                    issue_confirmation_token(&mut transaction, workspace_id, fan_upsert.fan.id)
                        .await
                        .map_err(map_lifecycle_error)?;
                self.append_confirmation_requested_outbox(
                    &mut transaction,
                    workspace_id,
                    command,
                    fan_upsert.fan.id,
                    &confirmation_token,
                )
                .await?;
            }
            let result = FanSignupResult {
                fan_id: fan_upsert.fan.id,
                status: FanStatus::Pending,
                referral_code: None,
                fan_session_token: None,
                confirmation_required: true,
                created: false,
            };
            self.complete_idempotency(
                &mut transaction,
                workspace_id,
                command,
                &request_hash,
                &result,
            )
            .await?;
            transaction.commit().await.map_err(StoreError::from_sqlx)?;
            return Ok(result);
        }

        self.ensure_campaign_is_active(&mut transaction, signup)
            .await?;
        self.append_consent(&mut transaction, workspace_id, fan_upsert.fan.id, command)
            .await?;
        let city_id = self
            .resolve_city(&mut transaction, signup.city_slug())
            .await?;
        let city_interest_created = self
            .insert_city_interest(&mut transaction, workspace_id, fan_upsert.fan.id, city_id)
            .await?;
        if fan_upsert.became_active {
            self.increment_city_aggregates_for_fan(
                &mut transaction,
                workspace_id,
                fan_upsert.fan.id,
            )
            .await?;
        } else if fan_upsert.fan.status == FanStatus::Active && city_interest_created {
            self.increment_city_aggregate(&mut transaction, workspace_id, city_id)
                .await?;
        }

        let claimed_referral = self
            .resolve_claimed_referral(&mut transaction, workspace_id, fan_upsert.fan.id, signup)
            .await?;
        self.insert_acquisition_event(
            &mut transaction,
            workspace_id,
            fan_upsert.fan.id,
            signup,
            command.request_id().as_str(),
            claimed_referral.as_ref(),
        )
        .await?;
        record_pending_signup_referral(
            &mut transaction,
            workspace_id,
            fan_upsert.fan.id,
            claimed_referral.as_ref().map(|row| row.id),
            claimed_referral.as_ref().map(|row| row.fan_id),
        )
        .await
        .map_err(map_referral_error)?;

        let (referral_code, fan_session_token, confirmation_required) =
            if fan_upsert.fan.status == FanStatus::Active {
                qualify_signup_referral_and_rewards(
                    &mut transaction,
                    workspace_id,
                    fan_upsert.fan.id,
                    claimed_referral.as_ref().map(|row| row.id),
                    claimed_referral.as_ref().map(|row| row.fan_id),
                    command.request_id().as_str(),
                )
                .await
                .map_err(map_referral_error)?;
                let referral_code = self
                    .load_or_create_referral_code(&mut transaction, workspace_id, fan_upsert.fan.id)
                    .await?;
                let fan_session_token =
                    issue_fan_session(&mut transaction, workspace_id, fan_upsert.fan.id)
                        .await
                        .map_err(map_referral_error)?;
                let unsubscribe_token = issue_fan_action_token(
                    &mut transaction,
                    workspace_id,
                    fan_upsert.fan.id,
                    "unsubscribe",
                    730,
                )
                .await
                .map_err(map_lifecycle_error)?;
                if fan_upsert.created || fan_upsert.became_active {
                    self.append_fan_active_outbox(
                        &mut transaction,
                        FanActiveOutboxArgs {
                            workspace_id,
                            command,
                            fan_id: fan_upsert.fan.id,
                            referral_code: &referral_code,
                            unsubscribe_token: &unsubscribe_token,
                            created: fan_upsert.created,
                        },
                    )
                    .await?;
                }
                (Some(referral_code), Some(fan_session_token), false)
            } else {
                let confirmation_token =
                    issue_confirmation_token(&mut transaction, workspace_id, fan_upsert.fan.id)
                        .await
                        .map_err(map_lifecycle_error)?;
                self.append_confirmation_requested_outbox(
                    &mut transaction,
                    workspace_id,
                    command,
                    fan_upsert.fan.id,
                    &confirmation_token,
                )
                .await?;
                (None, None, true)
            };

        let result = FanSignupResult {
            fan_id: fan_upsert.fan.id,
            status: fan_upsert.fan.status,
            referral_code,
            fan_session_token,
            confirmation_required,
            created: fan_upsert.created,
        };
        self.complete_idempotency(
            &mut transaction,
            workspace_id,
            command,
            &request_hash,
            &result,
        )
        .await?;

        transaction.commit().await.map_err(StoreError::from_sqlx)?;
        Ok(result)
    }

    async fn configure_transaction(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
    ) -> Result<(), StoreError> {
        let statement_timeout = duration_as_milliseconds(self.operation_timeout)?;
        let lock_timeout = duration_as_milliseconds(self.lock_timeout)?;
        sqlx::query(
            r#"
            SELECT
                set_config('statement_timeout', $1, true),
                set_config('lock_timeout', $2, true)
            "#,
        )
        .bind(format!("{statement_timeout}ms"))
        .bind(format!("{lock_timeout}ms"))
        .execute(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        Ok(())
    }

    async fn trusted_workspace_id_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
    ) -> Result<WorkspaceId, StoreError> {
        let id =
            sqlx::query_scalar::<_, Uuid>("SELECT id FROM workspaces WHERE slug = $1 FOR SHARE")
                .bind(self.workspace_slug.as_str())
                .fetch_optional(&mut **transaction)
                .await
                .map_err(StoreError::from_sqlx)?
                .ok_or(StoreError::NotFound)?;
        Ok(WorkspaceId::from_uuid(id))
    }

    async fn ensure_campaign_is_active(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        signup: &FanSignup,
    ) -> Result<(), StoreError> {
        let Some(campaign_id) = signup.campaign_id() else {
            return Ok(());
        };
        let exists = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM campaigns
                WHERE workspace_id = $1
                    AND id = $2
                    AND active
            )
            "#,
        )
        .bind(signup.workspace_id().into_uuid())
        .bind(campaign_id.into_uuid())
        .fetch_one(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        if !exists {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    async fn start_idempotency(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        command: &SignupFanCommand,
        request_hash: &[u8],
    ) -> Result<bool, StoreError> {
        let lease_milliseconds = duration_as_milliseconds(self.operation_timeout)?;
        sqlx::query(
            r#"
            DELETE FROM idempotency_keys
            WHERE workspace_id = $1
                AND scope = $2
                AND key = $3
                AND expires_at <= now()
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(IDEMPOTENCY_SCOPE)
        .bind(command.idempotency_key().as_str())
        .execute(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;

        let result = sqlx::query(
            r#"
            INSERT INTO idempotency_keys (
                workspace_id,
                scope,
                key,
                request_hash,
                state,
                lease_owner,
                lease_expires_at,
                expires_at
            )
            VALUES (
                $1,
                $2,
                $3,
                $4,
                'in_progress',
                $5,
                now() + ($6::bigint * interval '1 millisecond'),
                now() + ($7::bigint * interval '1 millisecond')
            )
            ON CONFLICT (workspace_id, scope, key) DO NOTHING
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(IDEMPOTENCY_SCOPE)
        .bind(command.idempotency_key().as_str())
        .bind(request_hash)
        .bind(command.request_id().as_str())
        .bind(lease_milliseconds)
        .bind(IDEMPOTENCY_RETENTION_MILLISECONDS)
        .execute(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        Ok(result.rows_affected() == 1)
    }

    async fn lock_idempotency(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        command: &SignupFanCommand,
    ) -> Result<IdempotencyRow, StoreError> {
        sqlx::query_as::<_, IdempotencyRow>(
            r#"
            SELECT
                request_hash,
                state,
                response_body,
                response_content_type,
                COALESCE(lease_expires_at <= now(), false) AS lease_expired
            FROM idempotency_keys
            WHERE workspace_id = $1
                AND scope = $2
                AND key = $3
            FOR UPDATE
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(IDEMPOTENCY_SCOPE)
        .bind(command.idempotency_key().as_str())
        .fetch_one(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)
    }

    async fn reclaim_idempotency(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        command: &SignupFanCommand,
    ) -> Result<(), StoreError> {
        let lease_milliseconds = duration_as_milliseconds(self.operation_timeout)?;
        let updated = sqlx::query(
            r#"
            UPDATE idempotency_keys
            SET
                lease_owner = $4,
                lease_expires_at =
                    now() + ($5::bigint * interval '1 millisecond')
            WHERE workspace_id = $1
                AND scope = $2
                AND key = $3
                AND state = 'in_progress'
                AND lease_expires_at <= now()
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(IDEMPOTENCY_SCOPE)
        .bind(command.idempotency_key().as_str())
        .bind(command.request_id().as_str())
        .bind(lease_milliseconds)
        .execute(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        if updated.rows_affected() != 1 {
            return Err(StoreError::Conflict);
        }
        Ok(())
    }

    async fn upsert_fan(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        signup: &FanSignup,
    ) -> Result<FanUpsert, StoreError> {
        let inserted = sqlx::query_as::<_, FanRow>(
            r#"
            INSERT INTO fans (
                workspace_id,
                normalized_email,
                display_name,
                locale,
                status
            )
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (workspace_id, normalized_email) DO NOTHING
            RETURNING id, status
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(signup.email().as_str())
        .bind(signup.display_name())
        .bind(signup.locale())
        .bind(if self.require_double_opt_in {
            "pending"
        } else {
            "active"
        })
        .fetch_optional(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;

        if let Some(row) = inserted {
            let fan: StoredFan = row.try_into()?;
            return Ok(FanUpsert {
                fan,
                created: true,
                became_active: false,
                already_active: false,
                already_pending: false,
            });
        }

        let existing = sqlx::query_as::<_, FanRow>(
            r#"
            SELECT id, status
            FROM fans
            WHERE workspace_id = $1
                AND normalized_email = $2
            FOR UPDATE
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(signup.email().as_str())
        .fetch_one(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        let existing: StoredFan = existing.try_into()?;
        if existing.status == FanStatus::Suppressed {
            return Err(StoreError::Conflict);
        }
        if existing.status == FanStatus::Active {
            return Ok(FanUpsert {
                fan: existing,
                created: false,
                became_active: false,
                already_active: true,
                already_pending: false,
            });
        }
        if existing.status == FanStatus::Pending {
            return Ok(FanUpsert {
                fan: existing,
                created: false,
                became_active: false,
                already_active: false,
                already_pending: true,
            });
        }

        let row = sqlx::query_as::<_, FanRow>(
            r#"
            UPDATE fans
            SET
                display_name = COALESCE($3, display_name),
                locale = COALESCE($4, locale),
                status = 'pending'
            WHERE workspace_id = $1
                AND id = $2
            RETURNING id, status
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(existing.id.into_uuid())
        .bind(signup.display_name())
        .bind(signup.locale())
        .fetch_one(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;

        let fan: StoredFan = row.try_into()?;
        let became_active = existing.status != FanStatus::Active && fan.status == FanStatus::Active;
        Ok(FanUpsert {
            fan,
            created: false,
            became_active,
            already_active: false,
            already_pending: false,
        })
    }

    async fn fan_action_resend_is_in_cooldown(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        fan_id: FanId,
        purpose: &str,
    ) -> Result<bool, StoreError> {
        sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM fan_action_tokens
                WHERE workspace_id = $1
                    AND fan_id = $2
                    AND purpose = $3
                    AND consumed_at IS NULL
                    AND expires_at > now()
                    AND created_at >
                        now() - ($4::bigint * interval '1 minute')
            )
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id.into_uuid())
        .bind(purpose)
        .bind(CONFIRMATION_RESEND_COOLDOWN_MINUTES)
        .fetch_one(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)
    }

    async fn append_consent(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        fan_id: FanId,
        command: &SignupFanCommand,
    ) -> Result<(), StoreError> {
        let consent = command.signup().consent();
        sqlx::query(
            r#"
            INSERT INTO fan_consents (
                workspace_id,
                fan_id,
                purpose,
                granted,
                policy_version,
                source,
                request_id
            )
            VALUES ($1, $2, 'marketing', $3, $4, $5, $6)
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id.into_uuid())
        .bind(consent.granted())
        .bind(consent.policy_version())
        .bind(consent.source())
        .bind(command.request_id().as_str())
        .execute(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        Ok(())
    }

    async fn resolve_city(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        city_slug: &CitySlug,
    ) -> Result<CityId, StoreError> {
        let id = sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT id
            FROM cities
            WHERE country_code = $1
                AND slug = $2
            "#,
        )
        .bind(self.default_country_code.as_str())
        .bind(city_slug.as_str())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?
        .ok_or(StoreError::NotFound)?;
        Ok(CityId::from_uuid(id))
    }

    async fn insert_city_interest(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        fan_id: FanId,
        city_id: CityId,
    ) -> Result<bool, StoreError> {
        let inserted = sqlx::query_scalar::<_, i32>(
            r#"
            INSERT INTO fan_city_interests (workspace_id, fan_id, city_id)
            VALUES ($1, $2, $3)
            ON CONFLICT (workspace_id, fan_id, city_id) DO NOTHING
            RETURNING 1
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id.into_uuid())
        .bind(city_id.into_uuid())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        Ok(inserted.is_some())
    }

    async fn increment_city_aggregate(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        city_id: CityId,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            INSERT INTO city_aggregates (
                workspace_id,
                city_id,
                confirmed_fan_count
            )
            VALUES ($1, $2, 1)
            ON CONFLICT (workspace_id, city_id) DO UPDATE
            SET
                confirmed_fan_count =
                    city_aggregates.confirmed_fan_count + 1,
                updated_at = now()
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(city_id.into_uuid())
        .execute(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        Ok(())
    }

    async fn increment_city_aggregates_for_fan(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        fan_id: FanId,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            INSERT INTO city_aggregates (
                workspace_id,
                city_id,
                confirmed_fan_count
            )
            SELECT
                workspace_id,
                city_id,
                1
            FROM fan_city_interests
            WHERE workspace_id = $1
                AND fan_id = $2
            ON CONFLICT (workspace_id, city_id) DO UPDATE
            SET
                confirmed_fan_count =
                    city_aggregates.confirmed_fan_count + 1,
                updated_at = now()
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id.into_uuid())
        .execute(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        Ok(())
    }

    async fn load_or_create_referral_code(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        fan_id: FanId,
    ) -> Result<ReferralCode, StoreError> {
        let existing = sqlx::query_scalar::<_, String>(
            r#"
            SELECT code
            FROM referral_codes
            WHERE workspace_id = $1
                AND fan_id = $2
                AND active
            ORDER BY created_at, id
            LIMIT 1
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id.into_uuid())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        if let Some(code) = existing {
            return ReferralCode::parse(code).map_err(|_| StoreError::Unexpected);
        }

        for _ in 0..3 {
            let inserted = sqlx::query_scalar::<_, String>(
                r#"
                INSERT INTO referral_codes (
                    workspace_id,
                    fan_id,
                    code
                )
                VALUES ($1, $2, encode(gen_random_bytes(18), 'hex'))
                ON CONFLICT DO NOTHING
                RETURNING code
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(fan_id.into_uuid())
            .fetch_optional(&mut **transaction)
            .await
            .map_err(StoreError::from_sqlx)?;
            if let Some(code) = inserted {
                return ReferralCode::parse(code).map_err(|_| StoreError::Unexpected);
            }

            let existing = sqlx::query_scalar::<_, String>(
                r#"
                SELECT code
                FROM referral_codes
                WHERE workspace_id = $1
                    AND fan_id = $2
                    AND active
                ORDER BY created_at, id
                LIMIT 1
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(fan_id.into_uuid())
            .fetch_optional(&mut **transaction)
            .await
            .map_err(StoreError::from_sqlx)?;
            if let Some(code) = existing {
                return ReferralCode::parse(code).map_err(|_| StoreError::Unexpected);
            }
        }

        Err(StoreError::Unavailable)
    }

    async fn resolve_claimed_referral(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        referred_fan_id: FanId,
        signup: &FanSignup,
    ) -> Result<Option<ReferralOwnerRow>, StoreError> {
        let Some(code) = signup.claimed_referral_code() else {
            return Ok(None);
        };
        let referral = sqlx::query_as::<_, ReferralOwnerRow>(
            r#"
            SELECT id, fan_id
            FROM referral_codes
            WHERE workspace_id = $1
                AND code = $2
                AND active
            FOR SHARE
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(code.as_str())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        let Some(referral) = referral else {
            return Ok(None);
        };
        if referral.fan_id == referred_fan_id.into_uuid() {
            return Ok(None);
        }
        Ok(Some(referral))
    }

    async fn insert_acquisition_event(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        fan_id: FanId,
        signup: &FanSignup,
        request_id: &str,
        referral: Option<&ReferralOwnerRow>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            INSERT INTO fan_acquisition_events (
                workspace_id,
                fan_id,
                campaign_id,
                anonymous_visitor_id,
                source,
                request_id,
                referral_code_id,
                referrer_fan_id,
                occurred_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now())
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id.into_uuid())
        .bind(signup.campaign_id().map(Into::<Uuid>::into))
        .bind(signup.visitor_id().map(Into::<Uuid>::into))
        .bind(signup.consent().source())
        .bind(request_id)
        .bind(referral.as_ref().map(|row| row.id))
        .bind(referral.map(|row| row.fan_id))
        .execute(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        Ok(())
    }

    async fn append_fan_active_outbox(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        args: FanActiveOutboxArgs<'_>,
    ) -> Result<(), StoreError> {
        let FanActiveOutboxArgs {
            workspace_id,
            command,
            fan_id,
            referral_code,
            unsubscribe_token,
            created,
        } = args;
        let signup = command.signup();
        let payload = json!({
            "workspace_id": workspace_id,
            "fan_id": fan_id,
            "email": signup.email().as_str(),
            "display_name": signup.display_name(),
            "locale": signup.locale(),
            "city_slug": signup.city_slug(),
            "campaign_id": signup.campaign_id(),
            "referral_code": referral_code,
            "unsubscribe_token": unsubscribe_token.as_str(),
            "policy_version": signup.consent().policy_version(),
        });
        let event_type = if created {
            "fan.created"
        } else {
            "fan.reactivated"
        };
        self.append_outbox(transaction, workspace_id, event_type, command, payload)
            .await
    }

    async fn append_confirmation_requested_outbox(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        command: &SignupFanCommand,
        fan_id: FanId,
        confirmation_token: &crowdrelay_domain::FanActionToken,
    ) -> Result<(), StoreError> {
        let signup = command.signup();
        self.append_outbox(
            transaction,
            workspace_id,
            "fan.confirmation_requested",
            command,
            json!({
                "workspace_id": workspace_id,
                "fan_id": fan_id,
                "email": signup.email().as_str(),
                "display_name": signup.display_name(),
                "locale": signup.locale(),
                "city_slug": signup.city_slug(),
                "confirmation_token": confirmation_token.as_str(),
                "policy_version": signup.consent().policy_version(),
            }),
        )
        .await
    }

    async fn append_session_requested_outbox(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        command: &SignupFanCommand,
        fan_id: FanId,
        recovery_token: &FanActionToken,
    ) -> Result<(), StoreError> {
        let signup = command.signup();
        self.append_outbox(
            transaction,
            workspace_id,
            "fan.session_requested",
            command,
            json!({
                "workspace_id": workspace_id,
                "fan_id": fan_id,
                "email": signup.email().as_str(),
                "display_name": signup.display_name(),
                "locale": signup.locale(),
                "session_recovery_token": recovery_token.as_str(),
            }),
        )
        .await
    }

    async fn append_outbox(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        event_type: &str,
        command: &SignupFanCommand,
        payload: serde_json::Value,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            INSERT INTO outbox_events (
                workspace_id, event_type, event_version, payload, request_id
            )
            VALUES ($1, $2, 1, $3, $4)
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(event_type)
        .bind(payload)
        .bind(command.request_id().as_str())
        .execute(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        Ok(())
    }

    async fn complete_idempotency(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        command: &SignupFanCommand,
        request_hash: &[u8],
        result: &FanSignupResult,
    ) -> Result<(), StoreError> {
        let response = self
            .sensitive_response_codec
            .encrypt(
                workspace_id,
                IDEMPOTENCY_SCOPE,
                command.idempotency_key().as_str(),
                result,
            )
            .map_err(|_| StoreError::Unexpected)?;
        let response_status = if result.confirmation_required {
            202
        } else if result.created {
            201
        } else {
            200
        };
        let updated = sqlx::query(
            r#"
            UPDATE idempotency_keys
            SET
                state = 'completed',
                lease_owner = NULL,
                lease_expires_at = NULL,
                response_status = $5,
                response_body = $6,
                response_content_type = $7,
                completed_at = now()
            WHERE workspace_id = $1
                AND scope = $2
                AND key = $3
                AND request_hash = $4
                AND state = 'in_progress'
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(IDEMPOTENCY_SCOPE)
        .bind(command.idempotency_key().as_str())
        .bind(request_hash)
        .bind(response_status)
        .bind(response)
        .bind(ENCRYPTED_JSON_CONTENT_TYPE)
        .execute(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        if updated.rows_affected() != 1 {
            return Err(StoreError::Conflict);
        }
        Ok(())
    }

    async fn refresh_completed_idempotency_response(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        command: &SignupFanCommand,
        request_hash: &[u8],
        result: &FanSignupResult,
    ) -> Result<(), StoreError> {
        let response = self
            .sensitive_response_codec
            .encrypt(
                workspace_id,
                IDEMPOTENCY_SCOPE,
                command.idempotency_key().as_str(),
                result,
            )
            .map_err(|_| StoreError::Unexpected)?;
        let updated = sqlx::query(
            r#"
            UPDATE idempotency_keys
            SET
                response_body = $5,
                response_content_type = $6
            WHERE workspace_id = $1
                AND scope = $2
                AND key = $3
                AND request_hash = $4
                AND state = 'completed'
                AND expires_at > now()
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(IDEMPOTENCY_SCOPE)
        .bind(command.idempotency_key().as_str())
        .bind(request_hash)
        .bind(response)
        .bind(ENCRYPTED_JSON_CONTENT_TYPE)
        .execute(&mut **transaction)
        .await
        .map_err(StoreError::from_sqlx)?;
        if updated.rows_affected() != 1 {
            return Err(StoreError::Conflict);
        }
        Ok(())
    }
}

#[async_trait]
impl AcquisitionRepository for PostgresAcquisitionRepository {
    async fn resolve_workspace(
        &self,
        slug: &WorkspaceSlug,
    ) -> Result<Option<WorkspaceId>, RepositoryError> {
        self.bounded(self.resolve_workspace_inner(slug))
            .await
            .map_err(Into::into)
    }

    async fn load_active_smart_links(&self) -> Result<Vec<ResolvedSmartLink>, RepositoryError> {
        self.bounded(self.load_active_smart_links_inner())
            .await
            .map_err(Into::into)
    }

    async fn persist_click_batch(&self, clicks: &[ClickEvent]) -> Result<(), RepositoryError> {
        self.bounded(self.persist_click_batch_inner(clicks))
            .await
            .map_err(Into::into)
    }

    async fn persist_fan_signup(
        &self,
        command: &SignupFanCommand,
    ) -> Result<FanSignupResult, RepositoryError> {
        self.bounded(self.persist_fan_signup_inner(command))
            .await
            .map_err(Into::into)
    }

    async fn list_city_signals(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
    ) -> Result<Vec<CitySignal>, RepositoryError> {
        self.bounded(self.list_city_signals_inner(workspace_id, limit))
            .await
            .map_err(Into::into)
    }
}

/// Non-blocking sender used directly by the redirect fast path.
#[derive(Clone, Debug)]
pub struct ClickBuffer {
    sender: mpsc::Sender<ClickEvent>,
    metrics: Arc<ClickBufferMetrics>,
}

impl ClickBuffer {
    /// Builds a fixed-capacity channel and its single batch-writing consumer.
    pub fn new(
        repository: Arc<dyn AcquisitionRepository>,
        config: crate::config::ClickBufferConfig,
    ) -> Result<(Self, ClickBatchWorker), ClickBufferBuildError> {
        if config.capacity == 0
            || config.batch_size == 0
            || config.batch_size > config.capacity
            || config.batch_size > MAX_CLICK_BATCH_ROWS
            || config.flush_interval.is_zero()
        {
            return Err(ClickBufferBuildError);
        }

        let (sender, receiver) = mpsc::channel(config.capacity);
        let metrics = Arc::new(ClickBufferMetrics::default());
        Ok((
            Self {
                sender,
                metrics: Arc::clone(&metrics),
            },
            ClickBatchWorker {
                receiver,
                repository,
                metrics,
                batch_size: config.batch_size,
                flush_interval: config.flush_interval,
            },
        ))
    }

    /// Attempts to queue a click without waiting or allocating an unbounded
    /// retry. Overload and a stopped consumer both drop analytics only.
    #[must_use]
    pub fn try_send(&self, event: ClickEvent) -> ClickEnqueueOutcome {
        match self.sender.try_send(event) {
            Ok(()) => {
                self.metrics.queued.fetch_add(1, Ordering::Relaxed);
                ClickEnqueueOutcome::Queued
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.metrics.dropped.fetch_add(1, Ordering::Relaxed);
                ClickEnqueueOutcome::DroppedFull
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.metrics.dropped.fetch_add(1, Ordering::Relaxed);
                ClickEnqueueOutcome::DroppedClosed
            }
        }
    }

    #[must_use]
    pub fn metrics(&self) -> Arc<ClickBufferMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClickEnqueueOutcome {
    Queued,
    DroppedFull,
    DroppedClosed,
}

#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("click buffer configuration is invalid")]
pub struct ClickBufferBuildError;

#[derive(Debug, Default)]
pub struct ClickBufferMetrics {
    queued: AtomicU64,
    persisted: AtomicU64,
    dropped: AtomicU64,
    persistence_failed: AtomicU64,
}

impl ClickBufferMetrics {
    #[must_use]
    pub fn snapshot(&self) -> ClickBufferSnapshot {
        ClickBufferSnapshot {
            queued: self.queued.load(Ordering::Relaxed),
            persisted: self.persisted.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            persistence_failed: self.persistence_failed.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ClickBufferSnapshot {
    pub queued: u64,
    pub persisted: u64,
    pub dropped: u64,
    pub persistence_failed: u64,
}

/// Cancellation-aware single consumer that persists fixed-size click batches.
pub struct ClickBatchWorker {
    receiver: mpsc::Receiver<ClickEvent>,
    repository: Arc<dyn AcquisitionRepository>,
    metrics: Arc<ClickBufferMetrics>,
    batch_size: usize,
    flush_interval: Duration,
}

impl ClickBatchWorker {
    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) {
        let mut batch = Vec::with_capacity(self.batch_size);
        let mut flush_interval = interval(self.flush_interval);
        flush_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        flush_interval.tick().await;

        if *shutdown.borrow() {
            self.shutdown(&mut batch).await;
            return;
        }

        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        self.shutdown(&mut batch).await;
                        return;
                    }
                }
                event = self.receiver.recv() => {
                    let Some(event) = event else {
                        self.flush(&mut batch).await;
                        return;
                    };
                    batch.push(event);
                    if batch.len() >= self.batch_size {
                        self.flush(&mut batch).await;
                    }
                }
                _ = flush_interval.tick() => {
                    self.flush(&mut batch).await;
                }
            }
        }
    }

    async fn shutdown(&mut self, batch: &mut Vec<ClickEvent>) {
        self.receiver.close();
        while batch.len() < self.batch_size {
            let Ok(event) = self.receiver.try_recv() else {
                break;
            };
            batch.push(event);
        }
        self.flush(batch).await;

        let mut dropped = 0_u64;
        while self.receiver.try_recv().is_ok() {
            dropped = dropped.saturating_add(1);
        }
        self.metrics.dropped.fetch_add(dropped, Ordering::Relaxed);
    }

    async fn flush(&self, batch: &mut Vec<ClickEvent>) {
        if batch.is_empty() {
            return;
        }
        let batch_len = u64::try_from(batch.len()).unwrap_or(u64::MAX);
        match self.repository.persist_click_batch(batch).await {
            Ok(()) => {
                self.metrics
                    .persisted
                    .fetch_add(batch_len, Ordering::Relaxed);
            }
            Err(_) => {
                self.metrics
                    .persistence_failed
                    .fetch_add(batch_len, Ordering::Relaxed);
                self.metrics.dropped.fetch_add(batch_len, Ordering::Relaxed);
                tracing::warn!(
                    batch_size = batch_len,
                    "click analytics batch persistence failed"
                );
            }
        }
        batch.clear();
    }
}

fn map_referral_error(error: crate::referrals::ReferralStoreError) -> StoreError {
    match error {
        crate::referrals::ReferralStoreError::Unavailable => StoreError::Unavailable,
        crate::referrals::ReferralStoreError::NotFound => StoreError::NotFound,
        crate::referrals::ReferralStoreError::Conflict => StoreError::Conflict,
        crate::referrals::ReferralStoreError::Unexpected => StoreError::Unexpected,
    }
}

fn map_lifecycle_error(error: crate::fan_lifecycle::LifecycleStoreError) -> StoreError {
    match error {
        crate::fan_lifecycle::LifecycleStoreError::Unavailable => StoreError::Unavailable,
        crate::fan_lifecycle::LifecycleStoreError::NotFound => StoreError::NotFound,
        crate::fan_lifecycle::LifecycleStoreError::Conflict => StoreError::Conflict,
        crate::fan_lifecycle::LifecycleStoreError::Unexpected => StoreError::Unexpected,
    }
}

#[derive(Debug, FromRow)]
struct SmartLinkRow {
    id: Uuid,
    workspace_id: Uuid,
    campaign_id: Option<Uuid>,
    slug: String,
    destination_url: String,
    version: i64,
}

impl TryFrom<SmartLinkRow> for ResolvedSmartLink {
    type Error = InvalidStoredData;

    fn try_from(row: SmartLinkRow) -> Result<Self, Self::Error> {
        let version = u64::try_from(row.version).map_err(|_| InvalidStoredData)?;
        ResolvedSmartLink::new(
            SmartLinkId::from_uuid(row.id),
            WorkspaceId::from_uuid(row.workspace_id),
            row.campaign_id.map(CampaignId::from_uuid),
            SmartLinkSlug::parse(row.slug).map_err(|_| InvalidStoredData)?,
            DestinationUrl::parse(row.destination_url).map_err(|_| InvalidStoredData)?,
            version,
        )
        .map_err(|_| InvalidStoredData)
    }
}

#[derive(Debug, FromRow)]
struct CitySignalRow {
    city_id: Uuid,
    slug: String,
    name: String,
    country_code: String,
    fan_count: i64,
}

impl TryFrom<CitySignalRow> for CitySignal {
    type Error = InvalidStoredData;

    fn try_from(row: CitySignalRow) -> Result<Self, Self::Error> {
        let fan_count = u64::try_from(row.fan_count).map_err(|_| InvalidStoredData)?;
        CitySignal::new(
            CityId::from_uuid(row.city_id),
            CitySlug::parse(row.slug).map_err(|_| InvalidStoredData)?,
            row.name,
            CountryCode::parse(row.country_code).map_err(|_| InvalidStoredData)?,
            fan_count,
        )
        .map_err(|_| InvalidStoredData)
    }
}

#[derive(Debug, FromRow)]
struct IdempotencyRow {
    request_hash: Vec<u8>,
    state: String,
    response_body: Option<serde_json::Value>,
    response_content_type: Option<String>,
    lease_expired: bool,
}

#[derive(Debug, FromRow)]
struct FanRow {
    id: Uuid,
    status: String,
}

#[derive(Clone, Copy, Debug)]
struct StoredFan {
    id: FanId,
    status: FanStatus,
}

#[derive(Clone, Copy, Debug)]
struct FanUpsert {
    fan: StoredFan,
    created: bool,
    became_active: bool,
    already_active: bool,
    already_pending: bool,
}

impl TryFrom<FanRow> for StoredFan {
    type Error = StoreError;

    fn try_from(row: FanRow) -> Result<Self, Self::Error> {
        let status = match row.status.as_str() {
            "pending" => FanStatus::Pending,
            "active" => FanStatus::Active,
            "unsubscribed" => FanStatus::Unsubscribed,
            "suppressed" => FanStatus::Suppressed,
            _ => return Err(StoreError::Unexpected),
        };
        Ok(Self {
            id: FanId::from_uuid(row.id),
            status,
        })
    }
}

#[derive(Clone, Copy, Debug, FromRow)]
struct ReferralOwnerRow {
    id: Uuid,
    fan_id: Uuid,
}

#[derive(Clone, Copy, Debug)]
struct InvalidStoredData;

#[derive(Clone, Copy, Debug)]
enum StoreError {
    Unavailable,
    NotFound,
    Conflict,
    Unexpected,
}

impl StoreError {
    fn from_sqlx(error: sqlx::Error) -> Self {
        match classify_sqlx_error(&error) {
            SqlxErrorClass::Unavailable => Self::Unavailable,
            SqlxErrorClass::NotFound => Self::NotFound,
            SqlxErrorClass::Conflict => Self::Conflict,
            SqlxErrorClass::Unexpected => Self::Unexpected,
        }
    }
}

impl From<StoreError> for RepositoryError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::Unavailable => Self::Unavailable,
            StoreError::NotFound => Self::NotFound,
            StoreError::Conflict => Self::Conflict,
            StoreError::Unexpected => Self::Unexpected,
        }
    }
}

fn duration_as_milliseconds(duration: Duration) -> Result<i64, StoreError> {
    i64::try_from(duration.as_millis()).map_err(|_| StoreError::Unexpected)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use crowdrelay_domain::VisitorId;

    use super::*;

    #[derive(Default)]
    struct FakeRepository {
        persisted: Mutex<Vec<Vec<ClickEvent>>>,
    }

    #[async_trait]
    impl AcquisitionRepository for FakeRepository {
        async fn resolve_workspace(
            &self,
            _slug: &WorkspaceSlug,
        ) -> Result<Option<WorkspaceId>, RepositoryError> {
            unreachable!("not used by click buffer tests")
        }

        async fn load_active_smart_links(&self) -> Result<Vec<ResolvedSmartLink>, RepositoryError> {
            unreachable!("not used by click buffer tests")
        }

        async fn persist_click_batch(&self, clicks: &[ClickEvent]) -> Result<(), RepositoryError> {
            self.persisted
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(clicks.to_vec());
            Ok(())
        }

        async fn persist_fan_signup(
            &self,
            _command: &SignupFanCommand,
        ) -> Result<FanSignupResult, RepositoryError> {
            unreachable!("not used by click buffer tests")
        }

        async fn list_city_signals(
            &self,
            _workspace_id: WorkspaceId,
            _limit: u32,
        ) -> Result<Vec<CitySignal>, RepositoryError> {
            unreachable!("not used by click buffer tests")
        }
    }

    fn click_event() -> Result<ClickEvent, Box<dyn std::error::Error>> {
        let link = ResolvedSmartLink::new(
            SmartLinkId::new(),
            WorkspaceId::new(),
            None,
            SmartLinkSlug::parse("tour")?,
            DestinationUrl::parse("https://virya.music/join")?,
            1,
        )?;
        Ok(ClickEvent::from_link(
            &link,
            Some(VisitorId::new()),
            Some("example.com".to_owned()),
            OffsetDateTime::UNIX_EPOCH,
        )?)
    }

    #[test]
    fn rejects_invalid_smart_link_rows_instead_of_loading_unsafe_urls() {
        let row = SmartLinkRow {
            id: Uuid::now_v7(),
            workspace_id: Uuid::now_v7(),
            campaign_id: None,
            slug: "safe-link".to_owned(),
            destination_url: "javascript:alert(1)".to_owned(),
            version: 1,
        };

        assert!(ResolvedSmartLink::try_from(row).is_err());
    }

    #[test]
    fn rejects_negative_database_counters() {
        let row = CitySignalRow {
            city_id: Uuid::now_v7(),
            slug: "wroclaw".to_owned(),
            name: "Wrocław".to_owned(),
            country_code: "PL".to_owned(),
            fan_count: -1,
        };

        assert!(CitySignal::try_from(row).is_err());
    }

    #[test]
    fn full_click_channel_drops_without_waiting() -> Result<(), Box<dyn std::error::Error>> {
        let repository = Arc::new(FakeRepository::default());
        let (buffer, _worker) = ClickBuffer::new(
            repository,
            crate::config::ClickBufferConfig {
                capacity: 1,
                batch_size: 1,
                flush_interval: Duration::from_secs(1),
            },
        )?;

        assert_eq!(buffer.try_send(click_event()?), ClickEnqueueOutcome::Queued);
        assert_eq!(
            buffer.try_send(click_event()?),
            ClickEnqueueOutcome::DroppedFull
        );
        assert_eq!(
            buffer.metrics().snapshot(),
            ClickBufferSnapshot {
                queued: 1,
                persisted: 0,
                dropped: 1,
                persistence_failed: 0,
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_flush_is_bounded_to_one_batch() -> Result<(), Box<dyn std::error::Error>> {
        let repository = Arc::new(FakeRepository::default());
        let (buffer, worker) = ClickBuffer::new(
            repository,
            crate::config::ClickBufferConfig {
                capacity: 4,
                batch_size: 2,
                flush_interval: Duration::from_secs(60),
            },
        )?;
        for _ in 0..4 {
            assert_eq!(buffer.try_send(click_event()?), ClickEnqueueOutcome::Queued);
        }
        let (shutdown_sender, shutdown) = watch::channel(true);
        worker.run(shutdown).await;
        drop(shutdown_sender);

        let snapshot = buffer.metrics().snapshot();
        assert_eq!(snapshot.queued, 4);
        assert_eq!(snapshot.persisted, 2);
        assert_eq!(snapshot.dropped, 2);
        assert_eq!(snapshot.persistence_failed, 0);
        Ok(())
    }
}
