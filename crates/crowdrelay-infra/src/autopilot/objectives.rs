//! Operator-declared targets, and where the series says they stand.
//!
//! The baseline is frozen when the target is declared, because progress from a
//! baseline that moves is not progress. Everything else is derived on read: a
//! stored "on track" goes stale silently and a derived one cannot.

use super::*;

#[derive(sqlx::FromRow)]
struct ObjectiveRow {
    id: Uuid,
    platform: String,
    metric_key: String,
    scope_kind: String,
    scope_id: Option<Uuid>,
    direction: String,
    baseline_value: i64,
    target_value: i64,
    declared_at: OffsetDateTime,
    deadline: OffsetDateTime,
    declared_by: String,
}

/// The latest value of one series, or nothing.
///
/// Reads the level rather than a trend: an objective is a target on a number,
/// and the number is what it is compared against.
async fn latest_series_value(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    platform: &str,
    metric_key: &str,
) -> Result<Option<(i64, OffsetDateTime)>, RepositoryError> {
    sqlx::query_as::<_, (i64, OffsetDateTime)>(
        r#"
        SELECT point.value, point.captured_at
        FROM viryaos_growth_metric_points AS point
        JOIN viryaos_growth_metric_series AS series
          ON series.workspace_id = point.workspace_id
         AND series.id = point.series_id
         AND series.active
        WHERE point.workspace_id = $1
          AND series.platform = $2
          AND series.metric_key = $3
        ORDER BY point.captured_at DESC
        LIMIT 1
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(platform)
    .bind(metric_key)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)
}

#[async_trait]
impl AutopilotObjectiveRepository for PostgresAutopilotRepository {
    async fn declare_growth_objective(
        &self,
        workspace_id: WorkspaceId,
        command: DeclareGrowthObjective,
        _idempotency_key: &IdempotencyKey,
        _request_id: Option<&RequestId>,
    ) -> Result<GrowthObjectiveMutation, RepositoryError> {
        self.bounded(async {
            if command.declared_by.trim().is_empty() {
                return Err(RepositoryError::Unexpected);
            }
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let observed = latest_series_value(
                &mut transaction,
                workspace_id,
                command.platform.as_str(),
                &command.metric_key,
            )
            .await?;
            // A series nobody is reading cannot carry a baseline. Zero would be
            // a claim about a number we have never seen, and every later
            // percentage would inherit it.
            //
            // Which is exactly what happened. The Spotify followers objective
            // was declared on 2026-08-24 and that series' first point is
            // 2026-08-31, so `unwrap_or(0)` below stored a baseline of 0
            // against a channel that already had 183 followers. Progress then
            // read 73% of the way to 250 while the true movement was zero —
            // the number has been flat at 183 the whole time.
            //
            // Refusing is better than guessing. An objective is a claim about
            // change, and there is no change to measure from a point nobody
            // has observed; the caller can declare it again once the sync has
            // produced one.
            let Some((baseline_value, _)) = observed else {
                return Err(RepositoryError::ConflictBecause(
                    "this metric has never been measured, so an objective \
                     declared now would take a baseline of zero and report \
                     progress it did not make; wait for the first sync",
                ));
            };
            let inserted = sqlx::query_as::<_, (Uuid, i64)>(
                r#"
                INSERT INTO viryaos_growth_objectives (
                    workspace_id, platform, metric_key, scope_kind, scope_id,
                    direction, baseline_value, target_value, deadline, declared_by
                )
                VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
                ON CONFLICT (workspace_id, platform, metric_key, scope_kind, scope_id)
                DO NOTHING
                RETURNING id, baseline_value
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.platform.as_str())
            .bind(&command.metric_key)
            .bind(command.scope.kind())
            .bind(command.scope.subject_id())
            .bind(command.direction.as_str())
            .bind(baseline_value)
            .bind(command.target_value)
            .bind(command.deadline)
            .bind(command.declared_by.trim())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;

            let (objective_id, stored_baseline, replayed) = match inserted {
                Some((id, stored)) => (id, stored, false),
                None => {
                    // A target already exists for this series and scope.
                    // Re-declaring returns it rather than opening a second one
                    // somebody could pick between.
                    let existing = sqlx::query_as::<_, (Uuid, i64)>(
                        r#"
                        SELECT id, baseline_value
                        FROM viryaos_growth_objectives
                        WHERE workspace_id = $1 AND platform = $2 AND metric_key = $3
                          AND scope_kind = $4 AND scope_id IS NOT DISTINCT FROM $5
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(command.platform.as_str())
                    .bind(&command.metric_key)
                    .bind(command.scope.kind())
                    .bind(command.scope.subject_id())
                    .fetch_one(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                    (existing.0, existing.1, true)
                }
            };
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(GrowthObjectiveMutation {
                operation_id: Uuid::now_v7(),
                objective_id,
                baseline_value: Some(stored_baseline),
                replayed,
            })
        })
        .await
    }

    async fn retire_growth_objective(
        &self,
        workspace_id: WorkspaceId,
        objective_id: Uuid,
        _idempotency_key: &IdempotencyKey,
        _request_id: Option<&RequestId>,
    ) -> Result<GrowthObjectiveMutation, RepositoryError> {
        self.bounded(async {
            // Retired, never deleted. A target that was declared and then
            // removed is exactly what a later review needs to see, and a
            // missing row cannot be reviewed.
            let updated = sqlx::query_as::<_, (i64,)>(
                r#"
                UPDATE viryaos_growth_objectives
                SET retired_at = now()
                WHERE workspace_id = $1 AND id = $2 AND retired_at IS NULL
                RETURNING baseline_value
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(objective_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(map_sqlx)?;
            let (baseline, replayed) = match updated {
                Some((baseline,)) => (baseline, false),
                None => {
                    let existing = sqlx::query_as::<_, (i64,)>(
                        "SELECT baseline_value FROM viryaos_growth_objectives
                         WHERE workspace_id=$1 AND id=$2",
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(objective_id)
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(map_sqlx)?
                    .ok_or(RepositoryError::NotFound)?;
                    (existing.0, true)
                }
            };
            Ok(GrowthObjectiveMutation {
                operation_id: Uuid::now_v7(),
                objective_id,
                baseline_value: Some(baseline),
                replayed,
            })
        })
        .await
    }

    async fn load_growth_objectives(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<GrowthObjectiveView>, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let rows = sqlx::query_as::<_, ObjectiveRow>(
                r#"
                SELECT id, platform, metric_key, scope_kind, scope_id, direction,
                       baseline_value, target_value, declared_at, deadline, declared_by
                FROM viryaos_growth_objectives
                WHERE workspace_id = $1 AND retired_at IS NULL
                ORDER BY deadline
                LIMIT 64
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_all(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            let policy = ObjectivePolicy::default();
            let mut views = Vec::with_capacity(rows.len());
            for row in rows {
                let observed = latest_series_value(
                    &mut transaction,
                    workspace_id,
                    &row.platform,
                    &row.metric_key,
                )
                .await?;
                let direction =
                    MetricDirection::parse(&row.direction).ok_or(RepositoryError::Unexpected)?;
                let objective = GrowthObjective {
                    platform: row.platform.clone(),
                    metric_key: row.metric_key.clone(),
                    scope: parse_objective_scope(&row.scope_kind, row.scope_id)?,
                    direction,
                    baseline_value: row.baseline_value,
                    target_value: row.target_value,
                    declared_at: row.declared_at,
                    deadline: row.deadline,
                };
                views.push(GrowthObjectiveView {
                    objective_id: row.id,
                    platform: row.platform,
                    metric_key: row.metric_key,
                    scope_kind: row.scope_kind,
                    scope_id: row.scope_id,
                    baseline_value: row.baseline_value,
                    target_value: row.target_value,
                    declared_at: row.declared_at,
                    deadline: row.deadline,
                    declared_by: row.declared_by,
                    observed_value: observed.map(|(value, _)| value),
                    state: assess_objective(&objective, observed, policy, now),
                });
            }
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(views)
        })
        .await
    }
}

fn parse_objective_scope(kind: &str, id: Option<Uuid>) -> Result<ObjectiveScope, RepositoryError> {
    match (kind, id) {
        ("workspace", None) => Ok(ObjectiveScope::Workspace),
        ("city", Some(id)) => Ok(ObjectiveScope::City(CityId::from_uuid(id))),
        ("event", Some(id)) => Ok(ObjectiveScope::Event(EventId::from_uuid(id))),
        ("release_plan", Some(id)) => Ok(ObjectiveScope::ReleasePlan(ReleasePlanId::from_uuid(id))),
        _ => Err(RepositoryError::Unexpected),
    }
}
