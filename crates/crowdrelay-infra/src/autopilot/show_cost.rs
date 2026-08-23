//! Freezing a show's predicted cost, and scoring it against what happened.
//!
//! The order is the whole design. A prediction is written once while the show
//! is ahead, together with the rates it came from; a settlement is written once
//! after it, and the verdict is derived in that same transaction. Neither write
//! can be repeated into a different answer, and a settlement with no prediction
//! behind it is refused rather than filled in.

use super::*;

#[derive(sqlx::FromRow)]
struct FrozenPredictionRow {
    offered_fee_minor: i64,
    predicted_transport_minor: Option<i64>,
    predicted_accommodation_minor: Option<i64>,
    predicted_per_diem_minor: Option<i64>,
    predicted_overhead_minor: Option<i64>,
    predicted_total_cost_minor: Option<i64>,
    predicted_round_trip_km: Option<i32>,
    settled_at: Option<OffsetDateTime>,
    accuracy: Option<String>,
    accuracy_reason: Option<String>,
}

#[derive(sqlx::FromRow)]
struct ShowCostLedgerRow {
    event_id: Uuid,
    event_title: String,
    starts_at: OffsetDateTime,
    predicted_at: OffsetDateTime,
    offered_fee_minor: i64,
    predicted_total_cost_minor: Option<i64>,
    predicted_net_margin_minor: Option<i64>,
    prediction_missing_input: Option<String>,
    settled_at: Option<OffsetDateTime>,
    settled_by: Option<String>,
    settled_total_cost_minor: Option<i64>,
    settled_net_margin_minor: Option<i64>,
    fee_received_minor: Option<i64>,
    accuracy: Option<String>,
    accuracy_reason: Option<String>,
    total_variance_basis_points: Option<i32>,
    worst_line: Option<String>,
    worst_line_delta_minor: Option<i64>,
    implied_transport_rate_minor_per_100km: Option<i64>,
}

/// Rebuilds the frozen estimate. Returns `None` when the prediction was an
/// honest refusal, which settles as `prediction_incomplete` rather than as a
/// model that was wrong.
fn frozen_cost(row: &FrozenPredictionRow) -> Option<ShowCost> {
    Some(ShowCost {
        // The basis is not re-derived: only the totals are compared, and
        // claiming a basis the frozen row does not carry would be a detail
        // nobody recorded.
        transport_basis: TransportBasis::FlatRate,
        transport_minor: row.predicted_transport_minor?,
        vehicles: 0,
        round_trip_km: u32::try_from(row.predicted_round_trip_km?).ok()?,
        nights_away: 0,
        rooms: 0,
        fuel_minor: 0,
        tolls_minor: 0,
        accommodation_minor: row.predicted_accommodation_minor?,
        per_diem_minor: row.predicted_per_diem_minor?,
        overhead_minor: row.predicted_overhead_minor?,
        total_cost_minor: row.predicted_total_cost_minor?,
        net_margin_minor: 0,
        walk_away_fee_minor: 0,
    })
}

impl PostgresAutopilotRepository {
    async fn settlement_policy(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
    ) -> Result<SettlementPolicy, RepositoryError> {
        // The tolerance lives with the rest of the tour economics config, which
        // is where an operator already goes to argue with the cost model.
        let stored = sqlx::query_scalar::<_, Value>(
            "SELECT settlement_policy FROM viryaos_tour_economics WHERE workspace_id = $1",
        )
        .bind(workspace_id.into_uuid())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(map_sqlx)?;
        Ok(stored
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default())
    }
}

#[async_trait]
impl AutopilotShowCostRepository for PostgresAutopilotRepository {
    async fn freeze_show_cost_prediction(
        &self,
        workspace_id: WorkspaceId,
        command: FreezeShowCostPrediction,
        _idempotency_key: &IdempotencyKey,
        _request_id: Option<&RequestId>,
    ) -> Result<ShowCostMutation, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            // The show has to exist, and it has to be ours.
            let known = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM events WHERE workspace_id=$1 AND id=$2)",
            )
            .bind(workspace_id.into_uuid())
            .bind(command.event_id.into_uuid())
            .fetch_one(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            if !known {
                return Err(RepositoryError::NotFound);
            }

            let policy = self.load_tour_economics(workspace_id).await?;
            let evidence = estimate_show_cost(
                &ShowLogistics {
                    distance_km: command.distance_km,
                    nights_away: command.nights_away,
                    offered_fee_minor: command.offered_fee_minor,
                    application_fee_minor: command.application_fee_minor,
                },
                &policy,
            );
            let cost = evidence.cost();
            let missing = match evidence {
                CostEvidence::Complete(_) => None,
                CostEvidence::Insufficient { missing } => Some(missing.as_str()),
            };

            let inserted = sqlx::query_scalar::<_, Uuid>(
                r#"
                INSERT INTO viryaos_show_cost_ledger (
                    workspace_id, event_id, predicted_at, tour_policy_snapshot,
                    distance_km, offered_fee_minor, application_fee_minor,
                    predicted_transport_minor, predicted_accommodation_minor,
                    predicted_per_diem_minor, predicted_overhead_minor,
                    predicted_total_cost_minor, predicted_net_margin_minor,
                    predicted_round_trip_km, prediction_missing_input
                )
                VALUES ($1,$2,now(),$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)
                ON CONFLICT (workspace_id, event_id) DO NOTHING
                RETURNING id
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.event_id.into_uuid())
            .bind(serde_json::to_value(policy).map_err(|_| RepositoryError::Unexpected)?)
            .bind(command.distance_km.and_then(|km| i32::try_from(km).ok()))
            .bind(command.offered_fee_minor)
            .bind(command.application_fee_minor)
            .bind(cost.map(|cost| cost.transport_minor))
            .bind(cost.map(|cost| cost.accommodation_minor))
            .bind(cost.map(|cost| cost.per_diem_minor))
            .bind(cost.map(|cost| cost.overhead_minor))
            .bind(cost.map(|cost| cost.total_cost_minor))
            .bind(cost.map(|cost| cost.net_margin_minor))
            .bind(cost.and_then(|cost| i32::try_from(cost.round_trip_km).ok()))
            .bind(missing)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(ShowCostMutation {
                operation_id: Uuid::now_v7(),
                event_id: command.event_id,
                accuracy: None,
                accuracy_reason: None,
                replayed: inserted.is_none(),
            })
        })
        .await
    }

    async fn settle_show_cost(
        &self,
        workspace_id: WorkspaceId,
        command: SettleShowCost,
        _idempotency_key: &IdempotencyKey,
        _request_id: Option<&RequestId>,
    ) -> Result<ShowCostMutation, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let frozen = sqlx::query_as::<_, FrozenPredictionRow>(
                r#"
                SELECT
                    offered_fee_minor,
                    predicted_transport_minor, predicted_accommodation_minor,
                    predicted_per_diem_minor, predicted_overhead_minor,
                    predicted_total_cost_minor, predicted_round_trip_km,
                    settled_at, accuracy, accuracy_reason
                FROM viryaos_show_cost_ledger
                WHERE workspace_id = $1 AND event_id = $2
                FOR UPDATE
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.event_id.into_uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?
            // No prediction was frozen. There is no honest way to score a model
            // against a show it was never asked about, so this is a refusal
            // rather than a backfill.
            .ok_or(RepositoryError::NotFound)?;

            // Already settled. The first account of what happened stands; a
            // second one would silently replace evidence somebody signed.
            if frozen.settled_at.is_some() {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(ShowCostMutation {
                    operation_id: Uuid::now_v7(),
                    event_id: command.event_id,
                    accuracy: frozen.accuracy,
                    accuracy_reason: frozen.accuracy_reason,
                    replayed: true,
                });
            }

            let policy = self
                .settlement_policy(&mut transaction, workspace_id)
                .await?;
            let settled = command.settled;
            let accuracy = frozen_cost(&frozen).map_or(
                ModelAccuracy::Insufficient {
                    reason: SettlementGap::PredictionIncomplete,
                },
                |predicted| {
                    assess_model_accuracy(predicted, frozen.offered_fee_minor, settled, policy)
                },
            );
            let (verdict, reason, variance, worst_line, worst_delta) = match accuracy {
                ModelAccuracy::Calibrated {
                    total_variance_basis_points,
                } => (
                    "calibrated",
                    None,
                    Some(total_variance_basis_points),
                    None,
                    None,
                ),
                ModelAccuracy::Drifting {
                    total_variance_basis_points,
                    worst_line,
                    worst_line_delta_minor,
                } => (
                    "drifting",
                    None,
                    Some(total_variance_basis_points),
                    Some(worst_line.as_str()),
                    Some(worst_line_delta_minor),
                ),
                ModelAccuracy::Insufficient { reason } => {
                    ("insufficient", Some(reason.as_str()), None, None, None)
                }
            };
            let implied = frozen
                .predicted_round_trip_km
                .and_then(|km| u32::try_from(km).ok())
                .and_then(|km| implied_transport_rate_minor_per_100km(settled.transport_minor, km));

            sqlx::query(
                r#"
                UPDATE viryaos_show_cost_ledger
                SET settled_at = now(),
                    settled_by = $3,
                    settled_transport_minor = $4,
                    settled_accommodation_minor = $5,
                    settled_per_diem_minor = $6,
                    settled_overhead_minor = $7,
                    settled_other_minor = $8,
                    fee_received_minor = $9,
                    settled_total_cost_minor = $10,
                    settled_net_margin_minor = $11,
                    accuracy = $12,
                    accuracy_reason = $13,
                    total_variance_basis_points = $14,
                    worst_line = $15,
                    worst_line_delta_minor = $16,
                    implied_transport_rate_minor_per_100km = $17
                WHERE workspace_id = $1 AND event_id = $2 AND settled_at IS NULL
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.event_id.into_uuid())
            .bind(&command.settled_by)
            .bind(settled.transport_minor)
            .bind(settled.accommodation_minor)
            .bind(settled.per_diem_minor)
            .bind(settled.overhead_minor)
            .bind(settled.other_minor)
            .bind(settled.fee_received_minor)
            .bind(settled.total_cost_minor())
            .bind(settled.net_margin_minor())
            .bind(verdict)
            .bind(reason)
            .bind(variance)
            .bind(worst_line)
            .bind(worst_delta)
            .bind(implied)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(ShowCostMutation {
                operation_id: Uuid::now_v7(),
                event_id: command.event_id,
                accuracy: Some(verdict.to_owned()),
                accuracy_reason: reason.map(ToOwned::to_owned),
                replayed: false,
            })
        })
        .await
    }

    async fn load_show_cost_ledger(
        &self,
        workspace_id: WorkspaceId,
        _now: OffsetDateTime,
    ) -> Result<Vec<ShowCostLedgerEntry>, RepositoryError> {
        self.bounded(async {
            let rows = sqlx::query_as::<_, ShowCostLedgerRow>(
                r#"
                SELECT
                    ledger.event_id, event.title AS event_title, event.starts_at,
                    ledger.predicted_at, ledger.offered_fee_minor,
                    ledger.predicted_total_cost_minor, ledger.predicted_net_margin_minor,
                    ledger.prediction_missing_input,
                    ledger.settled_at, ledger.settled_by,
                    ledger.settled_total_cost_minor, ledger.settled_net_margin_minor,
                    ledger.fee_received_minor,
                    ledger.accuracy, ledger.accuracy_reason,
                    ledger.total_variance_basis_points,
                    ledger.worst_line, ledger.worst_line_delta_minor,
                    ledger.implied_transport_rate_minor_per_100km
                FROM viryaos_show_cost_ledger AS ledger
                JOIN events AS event
                  ON event.workspace_id = ledger.workspace_id
                 AND event.id = ledger.event_id
                WHERE ledger.workspace_id = $1
                ORDER BY event.starts_at DESC
                LIMIT $2
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(MAX_SNAPSHOTS_PER_CONTEXT)
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
            Ok(rows
                .into_iter()
                .map(|row| {
                    let worst = row.worst_line.as_deref().and_then(CostLine::parse);
                    ShowCostLedgerEntry {
                        event_id: EventId::from_uuid(row.event_id),
                        event_title: row.event_title,
                        starts_at: row.starts_at,
                        predicted_at: row.predicted_at,
                        offered_fee_minor: row.offered_fee_minor,
                        predicted_total_cost_minor: row.predicted_total_cost_minor,
                        predicted_net_margin_minor: row.predicted_net_margin_minor,
                        prediction_missing_input: row.prediction_missing_input,
                        settled_at: row.settled_at,
                        settled_by: row.settled_by,
                        settled_total_cost_minor: row.settled_total_cost_minor,
                        settled_net_margin_minor: row.settled_net_margin_minor,
                        fee_received_minor: row.fee_received_minor,
                        accuracy: row.accuracy,
                        accuracy_reason: row.accuracy_reason,
                        total_variance_basis_points: row.total_variance_basis_points,
                        worst_line: row.worst_line,
                        worst_line_delta_minor: row.worst_line_delta_minor,
                        // The remedy travels with the finding. A variance an
                        // operator cannot act on is a number, not a finding.
                        worst_line_remedy: worst.map(CostLine::remedy),
                        implied_transport_rate_minor_per_100km: row
                            .implied_transport_rate_minor_per_100km,
                    }
                })
                .collect())
        })
        .await
    }
}
