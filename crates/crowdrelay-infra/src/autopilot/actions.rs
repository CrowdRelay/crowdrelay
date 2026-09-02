//! Split PostgreSQL Autopilot adapter implementation.

use super::*;

const TEAM_ASSIGNMENT_EMAIL_ACTION_KIND: &str = "team.assignment.email";

/// How long an approved action may sit `queued` while nobody advertises the
/// capability it needs. Executor registries are heartbeats, not promises: a
/// capability can disappear between approval and execution, and without this
/// grace window the action would rot in the queue forever — unclickable for
/// the operator, unclaimable for the worker, and a standing source of
/// executor-lag alerts.
const NO_EXECUTOR_GRACE: time::Duration = time::Duration::hours(24);

impl PostgresAutopilotRepository {
    /// Cancels approved actions that waited out the grace window while no
    /// live executor advertised the capability their payload needs.
    ///
    /// The capability mapping is applied in Rust — restating it in SQL would
    /// create a second authority the claim path could disagree with. The
    /// cancel itself mirrors the expired-approval sweep: terminal status,
    /// honest error kind, no retry.
    pub async fn cancel_unexecutable_actions(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<u64, RepositoryError> {
        self.bounded(async move {
            let stale: Vec<(Uuid, serde_json::Value)> = sqlx::query_as(
                r#"
                SELECT id, payload FROM viryaos_autopilot_actions
                WHERE workspace_id = $1 AND status = 'queued'
                  AND approved_at IS NOT NULL AND approved_at <= $2 - $3
                LIMIT 200
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .bind(NO_EXECUTOR_GRACE)
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;

            let live: Vec<String> = sqlx::query_scalar(
                r#"
                SELECT DISTINCT capability FROM viryaos_executor_capabilities
                WHERE workspace_id = $1 AND expires_at > $2
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;

            let mut cancelled = 0u64;
            for (id, payload) in stale {
                let Ok(parsed) = serde_json::from_value::<AutopilotActionPayload>(payload) else {
                    continue;
                };
                let Some(capability) = executor_capability_for_payload(&parsed) else {
                    continue;
                };
                if live.iter().any(|advertised| advertised == capability) {
                    continue;
                }
                // Name the capability. Cancelling silently leaves
                // `last_error_kind='no_executor'` in a table nobody reads and
                // no way to learn *which* executor is missing, so the brain
                // goes on deciding the same action, waiting out the same 24
                // hours and cancelling it again. Production has done this for
                // agent.content, beacon.outreach, outreach.send and
                // beacon.discovery — every one a decision spent on work
                // nothing could ever perform.
                tracing::warn!(
                    action_id = %id,
                    action_kind = parsed.action_kind(),
                    capability,
                    grace_hours = NO_EXECUTOR_GRACE.whole_hours(),
                    "cancelling autopilot action: no live executor advertises \
                     this capability. Until one does, every action of this \
                     kind will be decided, wait out the grace window, and be \
                     cancelled unexecuted"
                );
                cancelled += sqlx::query(
                    r#"
                    UPDATE viryaos_autopilot_actions
                    SET status = 'cancelled', finished_at = $3,
                        last_error_kind = 'no_executor'
                    WHERE workspace_id = $1 AND id = $2 AND status = 'queued'
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(id)
                .bind(now)
                .execute(&self.pool)
                .await
                .map_err(map_sqlx)?
                .rows_affected();
            }
            Ok(cancelled)
        })
        .await
    }

    pub async fn claim_due_autonomous_actions(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
        now: OffsetDateTime,
    ) -> Result<Vec<ClaimedAutopilotAction>, RepositoryError> {
        self.claim_due_actions_filtered(
            workspace_id,
            limit,
            now,
            None,
            Some(TEAM_ASSIGNMENT_EMAIL_ACTION_KIND),
        )
        .await
    }

    pub async fn claim_due_team_email_actions(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
        now: OffsetDateTime,
    ) -> Result<Vec<ClaimedAutopilotAction>, RepositoryError> {
        self.claim_due_actions_filtered(
            workspace_id,
            limit,
            now,
            Some(TEAM_ASSIGNMENT_EMAIL_ACTION_KIND),
            None,
        )
        .await
    }

    async fn claim_due_actions_filtered(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
        now: OffsetDateTime,
        include_action_kind: Option<&'static str>,
        exclude_action_kind: Option<&'static str>,
    ) -> Result<Vec<ClaimedAutopilotAction>, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            sqlx::query(
                r#"
                UPDATE viryaos_autopilot_actions
                SET status='cancelled', finished_at=$2, last_error_kind='approval_expired'
                WHERE workspace_id=$1 AND status='awaiting_approval'
                  AND ($3::text IS NULL OR action_kind = $3)
                  AND ($4::text IS NULL OR action_kind <> $4)
                  AND approval_expires_at IS NOT NULL AND approval_expires_at <= $2
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .bind(include_action_kind)
            .bind(exclude_action_kind)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            sqlx::query(
                r#"
                UPDATE viryaos_autopilot_actions
                SET status = 'failed',
                    finished_at = $2,
                    last_error_kind = 'stale_retry_exhausted'
                WHERE workspace_id = $1
                  AND status = 'processing'
                  AND ($3::text IS NULL OR action_kind = $3)
                  AND ($4::text IS NULL OR action_kind <> $4)
                  AND started_at <= $2 - INTERVAL '15 minutes'
                  AND attempt_count >= 5
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .bind(include_action_kind)
            .bind(exclude_action_kind)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            // Candidates are locked before they are claimed, because whether an
            // action can run at all depends on its payload: a capability nobody
            // advertises is an operator's decision, and claiming the action
            // anyway would spend one of its five attempts on a state that no
            // amount of retrying changes.
            let candidates = sqlx::query_as::<_, ClaimedActionRow>(
                r#"
                SELECT id, payload, attempt_count AS attempt_number
                FROM viryaos_autopilot_actions
                WHERE workspace_id = $1
                  AND attempt_count < 5
                  AND ($4::text IS NULL OR action_kind = $4)
                  AND ($5::text IS NULL OR action_kind <> $5)
                  AND (
                      (status = 'queued' AND available_at <= $2)
                      OR (
                          status = 'processing'
                          AND started_at <= $2 - INTERVAL '15 minutes'
                      )
                  )
                ORDER BY available_at, id
                FOR UPDATE SKIP LOCKED
                LIMIT $3
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .bind(i64::from(limit.min(100)))
            .bind(include_action_kind)
            .bind(exclude_action_kind)
            .fetch_all(&mut *transaction)
            .await
            .map_err(map_sqlx)?;

            let (runnable, parked) =
                partition_by_executor_capability(&mut transaction, workspace_id, candidates)
                    .await?;
            if !parked.is_empty() {
                park_gated_actions(&mut transaction, workspace_id, &parked, now).await?;
            }

            let rows = if runnable.is_empty() {
                Vec::new()
            } else {
                sqlx::query_as::<_, ClaimedActionRow>(
                    r#"
                    WITH claimed AS (
                        UPDATE viryaos_autopilot_actions AS action
                        SET status = 'processing',
                            attempt_count = action.attempt_count + 1,
                            started_at = $2,
                            last_error_kind = NULL
                        WHERE action.workspace_id = $1
                          AND action.id = ANY($3)
                        RETURNING action.id, action.payload, action.attempt_count
                    ), attempts AS (
                        INSERT INTO viryaos_autopilot_action_attempts (
                            workspace_id, action_id, attempt_number, outcome, occurred_at
                        )
                        SELECT $1, claimed.id, claimed.attempt_count, 'started', $2
                        FROM claimed
                        RETURNING action_id
                    )
                    SELECT claimed.id, claimed.payload, claimed.attempt_count AS attempt_number
                    FROM claimed
                    JOIN attempts ON attempts.action_id = claimed.id
                    ORDER BY claimed.id
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(now)
                .bind(&runnable)
                .fetch_all(&mut *transaction)
                .await
                .map_err(map_sqlx)?
            };

            let mut actions = Vec::with_capacity(rows.len());
            for row in rows {
                let payload = serde_json::from_value::<AutopilotActionPayload>(row.payload)
                    .map_err(|_| RepositoryError::Unexpected)?;
                let attempt_number =
                    u32::try_from(row.attempt_number).map_err(|_| RepositoryError::Unexpected)?;
                actions.push(ClaimedAutopilotAction {
                    id: AutopilotActionId::from_uuid(row.id),
                    payload,
                    attempt_number,
                });
            }
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(actions)
        })
        .await
    }
}

/// How long a gated action waits before it is looked at again. Long enough that
/// a permanently gated capability costs one check every few minutes instead of
/// one per poll, short enough that enabling the gate is felt quickly.
const GATED_ACTION_PARK: &str = "5 minutes";

/// Splits locked candidates into the ones an advertised executor can carry and
/// the ones waiting on a capability nobody offers.
async fn partition_by_executor_capability(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    candidates: Vec<ClaimedActionRow>,
) -> Result<(Vec<Uuid>, Vec<(Uuid, &'static str)>), RepositoryError> {
    if candidates.is_empty()
        || !super::executor_registry_is_active(transaction, workspace_id).await?
    {
        return Ok((
            candidates.into_iter().map(|row| row.id).collect(),
            Vec::new(),
        ));
    }

    let mut availability: Vec<(&'static str, bool)> = Vec::new();
    let mut runnable = Vec::with_capacity(candidates.len());
    let mut parked = Vec::new();
    for row in candidates {
        let payload = serde_json::from_value::<AutopilotActionPayload>(row.payload)
            .map_err(|_| RepositoryError::Unexpected)?;
        let Some(capability) = super::executor_capability_for_payload(&payload) else {
            runnable.push(row.id);
            continue;
        };
        let available = match availability.iter().find(|(name, _)| *name == capability) {
            Some((_, available)) => *available,
            None => {
                let available =
                    super::executor_capability_available(transaction, workspace_id, capability)
                        .await?;
                availability.push((capability, available));
                available
            }
        };
        if available {
            runnable.push(row.id);
        } else {
            parked.push((row.id, capability));
        }
    }
    Ok((runnable, parked))
}

/// Returns gated work to the queue without spending an attempt on it, and says
/// so once per cycle per capability, because that is a thing an operator can
/// act on. Retrying an operator's decision is not.
async fn park_gated_actions(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    parked: &[(Uuid, &'static str)],
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let ids: Vec<Uuid> = parked.iter().map(|(id, _)| *id).collect();
    sqlx::query(&format!(
        r#"
        UPDATE viryaos_autopilot_actions
        SET status = 'queued',
            started_at = NULL,
            available_at = $2 + INTERVAL '{GATED_ACTION_PARK}',
            last_error_kind = 'awaiting_executor'
        WHERE workspace_id = $1 AND id = ANY($3)
        "#
    ))
    .bind(workspace_id.into_uuid())
    .bind(now)
    .bind(&ids)
    .execute(&mut **transaction)
    .await
    .map_err(map_sqlx)?;

    let mut counted: Vec<(&'static str, usize)> = Vec::new();
    for (_, capability) in parked {
        match counted.iter_mut().find(|(name, _)| name == capability) {
            Some((_, count)) => *count += 1,
            None => counted.push((capability, 1)),
        }
    }
    for (capability, parked) in counted {
        tracing::warn!(
            workspace_id = %workspace_id.into_uuid(),
            capability,
            parked,
            "autopilot actions are parked: no executor advertises this capability"
        );
    }
    Ok(())
}

#[async_trait]
impl AutopilotActionRepository for PostgresAutopilotRepository {
    async fn claim_due_actions(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
        now: OffsetDateTime,
    ) -> Result<Vec<ClaimedAutopilotAction>, RepositoryError> {
        self.claim_due_actions_filtered(workspace_id, limit, now, None, None)
            .await
    }

    async fn execute_action(
        &self,
        workspace_id: WorkspaceId,
        action: &ClaimedAutopilotAction,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError> {
        self.execute_action_impl(workspace_id, action, now).await
    }

    async fn fail_action(
        &self,
        workspace_id: WorkspaceId,
        action_id: AutopilotActionId,
        error_kind: &'static str,
        retryable: bool,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let attempt = sqlx::query_scalar::<_, i32>(
                r#"
                UPDATE viryaos_autopilot_actions
                SET status = CASE
                        WHEN $5 AND attempt_count < 5 THEN 'queued'
                        ELSE 'failed'
                    END,
                    available_at = CASE
                        WHEN $5 AND attempt_count < 5 THEN $3 + INTERVAL '5 minutes'
                        ELSE available_at
                    END,
                    started_at = CASE
                        WHEN $5 AND attempt_count < 5 THEN NULL
                        ELSE started_at
                    END,
                    finished_at = CASE
                        WHEN $5 AND attempt_count < 5 THEN NULL
                        ELSE $3
                    END,
                    last_error_kind = $4
                WHERE workspace_id = $1 AND id = $2 AND status = 'processing'
                RETURNING attempt_count
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(action_id.into_uuid())
            .bind(now)
            .bind(error_kind)
            .bind(retryable)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            if let Some(attempt) = attempt {
                sqlx::query(
                    r#"
                    INSERT INTO viryaos_autopilot_action_attempts (
                        workspace_id, action_id, attempt_number, outcome, error_kind, occurred_at
                    ) VALUES ($1,$2,$3,'failed',$4,$5)
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(action_id.into_uuid())
                .bind(attempt)
                .bind(error_kind)
                .bind(now)
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
            }
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(())
        })
        .await
    }
}
