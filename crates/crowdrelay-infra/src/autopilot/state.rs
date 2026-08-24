//! Split PostgreSQL Autopilot adapter implementation.

use super::operator_actions::insert_operator_action;
use super::*;

#[async_trait]
impl AutopilotBookingStateRepository for PostgresAutopilotRepository {
    async fn upsert_booking_target(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertBookingTarget,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<BookingTargetMutation, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            if command.expected_version < 0
                || command.priority > 100
                || command.relationship_score > 100
                || command
                    .capacity
                    .is_some_and(|capacity| capacity == 0 || capacity > 100_000)
                || command.display_name.trim().is_empty()
                || command.contact_email.trim().is_empty()
            {
                return Err(RepositoryError::Unexpected);
            }
            if command.expected_version > 0 && command.target_id.is_none() {
                return Err(RepositoryError::Conflict);
            }
            let capacity = command
                .capacity
                .map(i32::try_from)
                .transpose()
                .map_err(|_| RepositoryError::Unexpected)?;

            let details = json!({
                "target_id": command.target_id,
                "city_id": command.city_id,
                "target_kind": command.kind.as_str(),
                "display_name": &command.display_name,
                "contact_email": &command.contact_email,
                "capacity": command.capacity,
                "priority": command.priority,
                "relationship_score": command.relationship_score,
                "active": command.active,
                "accepts_booking": command.accepts_booking,
                "expected_version": command.expected_version,
            });
            let operation_id = Uuid::now_v7();
            let proposed_target_id = command
                .target_id
                .unwrap_or_else(|| BookingTargetId::from_uuid(operation_id));
            let inserted_operation = sqlx::query_scalar::<_, Uuid>(
                r#"
                INSERT INTO operator_actions (
                    id, workspace_id, action, target_type, target_id,
                    idempotency_key, request_id, details
                ) VALUES ($1,$2,'upsert_autopilot_booking_target','booking_target',$3,$4,$5,$6)
                ON CONFLICT (workspace_id, idempotency_key) DO NOTHING
                RETURNING id
                "#,
            )
            .bind(operation_id)
            .bind(workspace_id.into_uuid())
            .bind(proposed_target_id.into_uuid())
            .bind(idempotency_key.as_str())
            .bind(request_id.map(RequestId::as_str))
            .bind(&details)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;

            if inserted_operation.is_none() {
                let existing = sqlx::query_as::<_, ExistingOperatorActionRow>(
                    r#"
                    SELECT id, action, target_type, target_id, details
                    FROM operator_actions
                    WHERE workspace_id = $1 AND idempotency_key = $2
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(idempotency_key.as_str())
                .fetch_one(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
                if existing.action != "upsert_autopilot_booking_target"
                    || existing.target_type != "booking_target"
                    || existing.details != details
                    || command
                        .target_id
                        .is_some_and(|target_id| target_id.into_uuid() != existing.target_id)
                {
                    return Err(RepositoryError::Conflict);
                }
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(BookingTargetMutation {
                    operation_id: existing.id,
                    target_id: BookingTargetId::from_uuid(existing.target_id),
                    version: command.expected_version.saturating_add(1).max(1),
                    replayed: true,
                });
            }

            let target_id = proposed_target_id;
            let version = if command.expected_version == 0 {
                sqlx::query_scalar::<_, i64>(
                    r#"
                    INSERT INTO viryaos_booking_targets (
                        id, workspace_id, city_id, target_kind, display_name, contact_email,
                        capacity, priority, relationship_score, active, accepts_booking
                    ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)
                    RETURNING version
                    "#,
                )
                .bind(target_id.into_uuid())
                .bind(workspace_id.into_uuid())
                .bind(command.city_id.into_uuid())
                .bind(command.kind.as_str())
                .bind(command.display_name.trim())
                .bind(command.contact_email.trim().to_ascii_lowercase())
                .bind(capacity)
                .bind(i32::from(command.priority))
                .bind(i32::from(command.relationship_score))
                .bind(command.active)
                .bind(command.accepts_booking)
                .fetch_one(&mut *transaction)
                .await
                .map_err(map_sqlx)?
            } else {
                sqlx::query_scalar::<_, i64>(
                    r#"
                    UPDATE viryaos_booking_targets
                    SET city_id = $3, target_kind = $4, display_name = $5, contact_email = $6,
                        capacity = $7, priority = $8, relationship_score = $9, active = $10,
                        accepts_booking = $11, version = version + 1
                    WHERE workspace_id = $1 AND id = $2 AND version = $12
                    RETURNING version
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(target_id.into_uuid())
                .bind(command.city_id.into_uuid())
                .bind(command.kind.as_str())
                .bind(command.display_name.trim())
                .bind(command.contact_email.trim().to_ascii_lowercase())
                .bind(capacity)
                .bind(i32::from(command.priority))
                .bind(i32::from(command.relationship_score))
                .bind(command.active)
                .bind(command.accepts_booking)
                .bind(command.expected_version)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(map_sqlx)?
                .ok_or(RepositoryError::Conflict)?
            };

            sqlx::query(
                r#"
                INSERT INTO viryaos_booking_target_history (
                    workspace_id, target_id, version, target_kind, display_name, contact_email,
                    capacity, priority, relationship_score, active, accepts_booking
                )
                SELECT workspace_id, id, version, target_kind, display_name, contact_email,
                       capacity, priority, relationship_score, active, accepts_booking
                FROM viryaos_booking_targets
                WHERE workspace_id = $1 AND id = $2 AND version = $3
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(target_id.into_uuid())
            .bind(version)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(BookingTargetMutation {
                operation_id,
                target_id,
                version,
                replayed: false,
            })
        })
        .await
    }
    async fn record_booking_reply(
        &self,
        workspace_id: WorkspaceId,
        command: crowdrelay_application::autopilot::RecordBookingReply,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        operations::record_booking_reply(self, workspace_id, command, idempotency_key, request_id)
            .await
    }
}

#[async_trait]
impl AutopilotMerchStateRepository for PostgresAutopilotRepository {
    async fn upsert_merch_product_economics(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertMerchProductEconomics,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<MerchProductEconomicsMutation, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;

            let operation_id = Uuid::now_v7();
            let details = json!({
                "minimum_price_minor": command.minimum_price_minor,
                "maximum_price_minor": command.maximum_price_minor,
                "unit_cost_minor": command.unit_cost_minor,
                "expected_version": command.expected_version,
            });
            let replay = insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "upsert_autopilot_merch_product_economics",
                "merch_product",
                command.product_id.into_uuid(),
                idempotency_key,
                request_id,
                &details,
            )
            .await?;
            if let Some(existing_operation_id) = replay {
                let version = command.expected_version.saturating_add(1);
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(MerchProductEconomicsMutation {
                    operation_id: existing_operation_id,
                    product_id: command.product_id,
                    version,
                    replayed: true,
                });
            }

            let current_price = sqlx::query_scalar::<_, i64>(
                r#"
                SELECT price_gross_minor
                FROM merch_products
                WHERE workspace_id = $1 AND id = $2
                FOR UPDATE
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.product_id.into_uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?
            .ok_or(RepositoryError::NotFound)?;
            if current_price < command.minimum_price_minor
                || current_price > command.maximum_price_minor
            {
                return Err(RepositoryError::Conflict);
            }

            let version = if command.expected_version == 0 {
                sqlx::query_scalar::<_, i64>(
                    r#"
                    INSERT INTO viryaos_merch_product_economics (
                        workspace_id, product_id, minimum_price_minor, maximum_price_minor,
                        unit_cost_minor, version
                    ) VALUES ($1,$2,$3,$4,$5,1)
                    ON CONFLICT (workspace_id, product_id) DO NOTHING
                    RETURNING version
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(command.product_id.into_uuid())
                .bind(command.minimum_price_minor)
                .bind(command.maximum_price_minor)
                .bind(command.unit_cost_minor)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(map_sqlx)?
                .ok_or(RepositoryError::Conflict)?
            } else {
                sqlx::query_scalar::<_, i64>(
                    r#"
                    UPDATE viryaos_merch_product_economics
                    SET minimum_price_minor = $3,
                        maximum_price_minor = $4,
                        unit_cost_minor = $5,
                        version = version + 1
                    WHERE workspace_id = $1 AND product_id = $2 AND version = $6
                    RETURNING version
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(command.product_id.into_uuid())
                .bind(command.minimum_price_minor)
                .bind(command.maximum_price_minor)
                .bind(command.unit_cost_minor)
                .bind(command.expected_version)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(map_sqlx)?
                .ok_or(RepositoryError::Conflict)?
            };

            sqlx::query(
                r#"
                INSERT INTO viryaos_merch_product_economics_history (
                    workspace_id, product_id, minimum_price_minor, maximum_price_minor,
                    unit_cost_minor, version
                ) VALUES ($1,$2,$3,$4,$5,$6)
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.product_id.into_uuid())
            .bind(command.minimum_price_minor)
            .bind(command.maximum_price_minor)
            .bind(command.unit_cost_minor)
            .bind(version)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(MerchProductEconomicsMutation {
                operation_id,
                product_id: command.product_id,
                version,
                replayed: false,
            })
        })
        .await
    }
}

#[async_trait]
impl AutopilotMarketStateRepository for PostgresAutopilotRepository {
    async fn upsert_promotion_budget_guardrail(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertPromotionBudgetGuardrail,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<PromotionBudgetGuardrailMutation, RepositoryError> {
        self.bounded(async {
            if command.expected_version < 0
                || command.maximum_total_daily_budget_minor <= 0
                || command.maximum_monthly_spend_minor <= 0
                || command.currency.len() != 3
                || !command.currency.bytes().all(|byte| byte.is_ascii_uppercase())
            {
                return Err(RepositoryError::Unexpected);
            }

            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let operation_id = Uuid::now_v7();
            let details = json!({
                "currency": &command.currency,
                "maximum_total_daily_budget_minor": command.maximum_total_daily_budget_minor,
                "maximum_monthly_spend_minor": command.maximum_monthly_spend_minor,
                "expected_version": command.expected_version,
            });
            let replay = insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "upsert_autopilot_promotion_budget_guardrail",
                "promotion_budget_guardrail",
                workspace_id.into_uuid(),
                idempotency_key,
                request_id,
                &details,
            )
            .await?;
            if let Some(existing_operation_id) = replay {
                let version = sqlx::query_scalar::<_, i64>(
                    "SELECT version FROM viryaos_promotion_budget_guardrails WHERE workspace_id = $1 AND currency = $2",
                )
                .bind(workspace_id.into_uuid())
                .bind(&command.currency)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(map_sqlx)?
                .ok_or(RepositoryError::Conflict)?;
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(PromotionBudgetGuardrailMutation {
                    operation_id: existing_operation_id,
                    currency: command.currency,
                    version,
                    replayed: true,
                });
            }

            let version = if command.expected_version == 0 {
                sqlx::query_scalar::<_, i64>(
                    r#"
                    INSERT INTO viryaos_promotion_budget_guardrails (
                        workspace_id, currency, maximum_total_daily_budget_minor,
                        maximum_monthly_spend_minor
                    ) VALUES ($1,$2,$3,$4)
                    RETURNING version
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(&command.currency)
                .bind(command.maximum_total_daily_budget_minor)
                .bind(command.maximum_monthly_spend_minor)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(map_sqlx)?
                .ok_or(RepositoryError::Conflict)?
            } else {
                sqlx::query_scalar::<_, i64>(
                    r#"
                    UPDATE viryaos_promotion_budget_guardrails
                    SET maximum_total_daily_budget_minor = $3,
                        maximum_monthly_spend_minor = $4,
                        version = version + 1
                    WHERE workspace_id = $1 AND currency = $2 AND version = $5
                    RETURNING version
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(&command.currency)
                .bind(command.maximum_total_daily_budget_minor)
                .bind(command.maximum_monthly_spend_minor)
                .bind(command.expected_version)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(map_sqlx)?
                .ok_or(RepositoryError::Conflict)?
            };

            sqlx::query(
                r#"
                INSERT INTO viryaos_promotion_budget_guardrail_history (
                    workspace_id, currency, version, maximum_total_daily_budget_minor,
                    maximum_monthly_spend_minor
                )
                SELECT workspace_id, currency, version, maximum_total_daily_budget_minor,
                       maximum_monthly_spend_minor
                FROM viryaos_promotion_budget_guardrails
                WHERE workspace_id = $1 AND currency = $2 AND version = $3
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(&command.currency)
            .bind(version)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(PromotionBudgetGuardrailMutation {
                operation_id,
                currency: command.currency,
                version,
                replayed: false,
            })
        })
        .await
    }

    async fn upsert_promotion_campaign_state(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertPromotionCampaignState,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<PromotionCampaignStateMutation, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;

            let existing_campaign_id = sqlx::query_scalar::<_, Uuid>(
                r#"
                SELECT id
                FROM viryaos_promotion_campaign_states
                WHERE workspace_id = $1 AND provider = $2 AND external_campaign_key = $3
                FOR UPDATE
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(&command.provider)
            .bind(&command.external_campaign_key)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            let campaign_id = existing_campaign_id.unwrap_or_else(Uuid::now_v7);
            let operation_id = Uuid::now_v7();
            let details = json!({
                "provider": &command.provider,
                "external_campaign_key": &command.external_campaign_key,
                "event_id": command.event_id.map(EventId::into_uuid),
                "currency": &command.currency,
                "current_daily_budget_minor": command.current_daily_budget_minor,
                "minimum_daily_budget_minor": command.minimum_daily_budget_minor,
                "maximum_daily_budget_minor": command.maximum_daily_budget_minor,
                "spend_last_7d_minor": command.spend_last_7d_minor,
                "spend_month_to_date_minor": command.spend_month_to_date_minor,
                "attributed_revenue_last_7d_minor": command.attributed_revenue_last_7d_minor,
                "active": command.active,
                "last_budget_change_at": command.last_budget_change_at,
                "observed_at": command.observed_at,
                "expires_at": command.expires_at,
            });
            let replay = insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "upsert_autopilot_promotion_state",
                "promotion_campaign",
                campaign_id,
                idempotency_key,
                request_id,
                &details,
            )
            .await?;
            if let Some(existing_operation_id) = replay {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(PromotionCampaignStateMutation {
                    operation_id: existing_operation_id,
                    campaign_id: PromotionCampaignId::from_uuid(campaign_id),
                    replayed: true,
                });
            }

            let upserted = sqlx::query_scalar::<_, Uuid>(
                r#"
                INSERT INTO viryaos_promotion_campaign_states (
                    id, workspace_id, provider, external_campaign_key, event_id, currency,
                    current_daily_budget_minor, minimum_daily_budget_minor, maximum_daily_budget_minor,
                    spend_last_7d_minor, spend_month_to_date_minor, attributed_revenue_last_7d_minor, active,
                    last_budget_change_at, observed_at, expires_at
                ) VALUES (
                    $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16
                )
                ON CONFLICT (workspace_id, provider, external_campaign_key) DO UPDATE
                SET event_id = EXCLUDED.event_id,
                    currency = EXCLUDED.currency,
                    current_daily_budget_minor = EXCLUDED.current_daily_budget_minor,
                    minimum_daily_budget_minor = EXCLUDED.minimum_daily_budget_minor,
                    maximum_daily_budget_minor = EXCLUDED.maximum_daily_budget_minor,
                    spend_last_7d_minor = EXCLUDED.spend_last_7d_minor,
                    spend_month_to_date_minor = EXCLUDED.spend_month_to_date_minor,
                    attributed_revenue_last_7d_minor = EXCLUDED.attributed_revenue_last_7d_minor,
                    active = EXCLUDED.active,
                    last_budget_change_at = EXCLUDED.last_budget_change_at,
                    observed_at = EXCLUDED.observed_at,
                    expires_at = EXCLUDED.expires_at,
                    updated_at = now()
                WHERE EXCLUDED.observed_at > viryaos_promotion_campaign_states.observed_at
                RETURNING id
                "#,
            )
            .bind(campaign_id)
            .bind(workspace_id.into_uuid())
            .bind(&command.provider)
            .bind(&command.external_campaign_key)
            .bind(command.event_id.map(EventId::into_uuid))
            .bind(&command.currency)
            .bind(command.current_daily_budget_minor)
            .bind(command.minimum_daily_budget_minor)
            .bind(command.maximum_daily_budget_minor)
            .bind(command.spend_last_7d_minor)
            .bind(command.spend_month_to_date_minor)
            .bind(command.attributed_revenue_last_7d_minor)
            .bind(command.active)
            .bind(command.last_budget_change_at)
            .bind(command.observed_at)
            .bind(command.expires_at)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;

            let persisted_id = upserted.ok_or(RepositoryError::Conflict)?;
            if persisted_id != campaign_id {
                return Err(RepositoryError::Conflict);
            }
            sqlx::query(
                r#"
                INSERT INTO viryaos_promotion_campaign_observations (
                    workspace_id, campaign_id, current_daily_budget_minor,
                    spend_last_7d_minor, spend_month_to_date_minor, attributed_revenue_last_7d_minor, active,
                    observed_at, expires_at
                ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(campaign_id)
            .bind(command.current_daily_budget_minor)
            .bind(command.spend_last_7d_minor)
            .bind(command.spend_month_to_date_minor)
            .bind(command.attributed_revenue_last_7d_minor)
            .bind(command.active)
            .bind(command.observed_at)
            .bind(command.expires_at)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(PromotionCampaignStateMutation {
                operation_id,
                campaign_id: PromotionCampaignId::from_uuid(campaign_id),
                replayed: false,
            })
        })
        .await
    }

    async fn upsert_city_market_signal(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertCityMarketSignal,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<CityMarketSignalMutation, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;

            let city_exists =
                sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM cities WHERE id = $1)")
                    .bind(command.city_id.into_uuid())
                    .fetch_one(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
            if !city_exists {
                return Err(RepositoryError::NotFound);
            }

            let existing_signal_id = sqlx::query_scalar::<_, Uuid>(
                r#"
                SELECT id
                FROM viryaos_city_market_signals
                WHERE workspace_id = $1
                  AND source = $2
                  AND city_id = $3
                  AND signal_kind = $4
                FOR UPDATE
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(&command.source)
            .bind(command.city_id.into_uuid())
            .bind(command.kind.as_str())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            let signal_id = existing_signal_id.unwrap_or_else(Uuid::now_v7);
            let operation_id = Uuid::now_v7();
            let details = json!({
                "source": &command.source,
                "city_id": command.city_id.into_uuid(),
                "signal_kind": command.kind.as_str(),
                "score_basis_points": command.score_basis_points,
                "confidence_basis_points": command.confidence.basis_points(),
                "observed_at": command.observed_at,
                "expires_at": command.expires_at,
            });
            let replay = insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "upsert_autopilot_city_market_signal",
                "city_market_signal",
                signal_id,
                idempotency_key,
                request_id,
                &details,
            )
            .await?;
            if let Some(existing_operation_id) = replay {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(CityMarketSignalMutation {
                    operation_id: existing_operation_id,
                    signal_id: MarketSignalId::from_uuid(signal_id),
                    replayed: true,
                });
            }

            let upserted = sqlx::query_scalar::<_, Uuid>(
                r#"
                INSERT INTO viryaos_city_market_signals (
                    id, workspace_id, source, city_id, signal_kind, score_basis_points,
                    confidence_basis_points, observed_at, expires_at
                ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
                ON CONFLICT (workspace_id, source, city_id, signal_kind) DO UPDATE
                SET score_basis_points = EXCLUDED.score_basis_points,
                    confidence_basis_points = EXCLUDED.confidence_basis_points,
                    observed_at = EXCLUDED.observed_at,
                    expires_at = EXCLUDED.expires_at,
                    updated_at = now()
                WHERE EXCLUDED.observed_at > viryaos_city_market_signals.observed_at
                RETURNING id
                "#,
            )
            .bind(signal_id)
            .bind(workspace_id.into_uuid())
            .bind(&command.source)
            .bind(command.city_id.into_uuid())
            .bind(command.kind.as_str())
            .bind(i32::from(command.score_basis_points))
            .bind(i32::from(command.confidence.basis_points()))
            .bind(command.observed_at)
            .bind(command.expires_at)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            let persisted_id = upserted.ok_or(RepositoryError::Conflict)?;
            if persisted_id != signal_id {
                return Err(RepositoryError::Conflict);
            }
            sqlx::query(
                r#"
                INSERT INTO viryaos_city_market_signal_observations (
                    workspace_id, signal_id, score_basis_points, confidence_basis_points,
                    observed_at, expires_at
                ) VALUES ($1,$2,$3,$4,$5,$6)
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(signal_id)
            .bind(i32::from(command.score_basis_points))
            .bind(i32::from(command.confidence.basis_points()))
            .bind(command.observed_at)
            .bind(command.expires_at)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(CityMarketSignalMutation {
                operation_id,
                signal_id: MarketSignalId::from_uuid(signal_id),
                replayed: false,
            })
        })
        .await
    }
}

#[async_trait]
impl AutopilotTicketStateRepository for PostgresAutopilotRepository {
    async fn upsert_ticket_allocation_guardrail(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertTicketAllocationGuardrail,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<TicketAllocationGuardrailMutation, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let operation_id = Uuid::now_v7();
            let details = json!({
                "minimum_capacity": command.minimum_capacity,
                "maximum_capacity": command.maximum_capacity,
                "step_capacity": command.step_capacity,
                "expected_version": command.expected_version,
            });
            if let Some(existing_operation_id) = insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "upsert_autopilot_ticket_allocation_guardrail",
                "ticket_type",
                command.ticket_type_id.into_uuid(),
                idempotency_key,
                request_id,
                &details,
            )
            .await?
            {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(TicketAllocationGuardrailMutation {
                    operation_id: existing_operation_id,
                    ticket_type_id: command.ticket_type_id,
                    version: command.expected_version.saturating_add(1).max(1),
                    replayed: true,
                });
            }

            let ticket = sqlx::query_as::<_, (Option<i32>, i32)>(
                r#"
                SELECT ticket_type.capacity, ticket_sale.capacity
                FROM ticket_types AS ticket_type
                JOIN ticket_sales AS ticket_sale
                  ON ticket_sale.workspace_id = ticket_type.workspace_id
                 AND ticket_sale.id = ticket_type.ticket_sale_id
                WHERE ticket_type.workspace_id = $1
                  AND ticket_type.id = $2
                  AND ticket_type.active
                  AND ticket_sale.active
                FOR UPDATE OF ticket_type
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.ticket_type_id.into_uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?
            .ok_or(RepositoryError::NotFound)?;
            let current_capacity = ticket.0.ok_or(RepositoryError::Conflict)?;
            let minimum =
                i32::try_from(command.minimum_capacity).map_err(|_| RepositoryError::Unexpected)?;
            let maximum =
                i32::try_from(command.maximum_capacity).map_err(|_| RepositoryError::Unexpected)?;
            let step =
                i32::try_from(command.step_capacity).map_err(|_| RepositoryError::Unexpected)?;
            if minimum <= 0
                || maximum < minimum
                || step <= 0
                || minimum > current_capacity
                || maximum < current_capacity
                || maximum > ticket.1
                || step > maximum
            {
                return Err(RepositoryError::Conflict);
            }

            let version = if command.expected_version == 0 {
                sqlx::query_scalar::<_, i64>(
                    r#"
                    INSERT INTO viryaos_ticket_type_allocation_guardrails (
                        workspace_id, ticket_type_id, minimum_capacity,
                        maximum_capacity, step_capacity, version
                    ) VALUES ($1,$2,$3,$4,$5,1)
                    ON CONFLICT (workspace_id, ticket_type_id) DO NOTHING
                    RETURNING version
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(command.ticket_type_id.into_uuid())
                .bind(minimum)
                .bind(maximum)
                .bind(step)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(map_sqlx)?
                .ok_or(RepositoryError::Conflict)?
            } else {
                sqlx::query_scalar::<_, i64>(
                    r#"
                    UPDATE viryaos_ticket_type_allocation_guardrails
                    SET minimum_capacity = $3,
                        maximum_capacity = $4,
                        step_capacity = $5,
                        version = version + 1
                    WHERE workspace_id = $1
                      AND ticket_type_id = $2
                      AND version = $6
                    RETURNING version
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(command.ticket_type_id.into_uuid())
                .bind(minimum)
                .bind(maximum)
                .bind(step)
                .bind(command.expected_version)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(map_sqlx)?
                .ok_or(RepositoryError::Conflict)?
            };
            sqlx::query(
                r#"
                INSERT INTO viryaos_ticket_type_allocation_guardrail_history (
                    workspace_id, ticket_type_id, version, minimum_capacity,
                    maximum_capacity, step_capacity
                ) VALUES ($1,$2,$3,$4,$5,$6)
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.ticket_type_id.into_uuid())
            .bind(version)
            .bind(minimum)
            .bind(maximum)
            .bind(step)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(TicketAllocationGuardrailMutation {
                operation_id,
                ticket_type_id: command.ticket_type_id,
                version,
                replayed: false,
            })
        })
        .await
    }
}
