//! PostgreSQL ecosystem control-plane repository.
//!
//! Owns the flag row, the advisory lock that serializes a replay window, and
//! the `operator_actions` audit row. All three commit together: an accepted
//! flag flip is always auditable, and a replay never writes a second time.

use async_trait::async_trait;
use crowdrelay_application::{
    EcosystemControlPlaneRepository, EcosystemRepositoryError, FeatureFlagMutation,
    FeatureFlagState, ReconciliationFindingState, ReconciliationOutcome, ReconciliationRunState,
    RunReconciliationCommand, ShowChecklistItemState, ShowChecklistMutation,
    UpdateFeatureFlagCommand, UpdateShowChecklistCommand,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use uuid::Uuid;

const FLAG_ACTION: &str = "feature_flag.updated";
const FLAG_TARGET_TYPE: &str = "feature_flag";
const CHECKLIST_ACTION: &str = "show_checklist.updated";
const CHECKLIST_TARGET_TYPE: &str = "show_checklist";
const RECONCILE_ACTION: &str = "reconciliation.run";
const RECONCILE_TARGET_TYPE: &str = "reconciliation";

/// PostgreSQL implementation of the ecosystem control-plane port.
#[derive(Clone)]
pub struct PostgresEcosystemRepository {
    pool: PgPool,
}

#[derive(FromRow)]
struct FlagRow {
    key: String,
    enabled: bool,
    reason: Option<String>,
    version: i64,
    updated_at: time::OffsetDateTime,
}

#[derive(FromRow)]
struct ChecklistRow {
    item_key: String,
    status: String,
    note: Option<String>,
    updated_at: time::OffsetDateTime,
}

#[derive(FromRow)]
struct ReconciliationRunRow {
    id: Uuid,
    status: String,
    trigger: String,
    finding_count: i32,
    started_at: time::OffsetDateTime,
    finished_at: Option<time::OffsetDateTime>,
}

#[derive(FromRow)]
struct ReconciliationFindingRow {
    id: Uuid,
    run_id: Uuid,
    kind: String,
    severity: String,
    entity_type: String,
    entity_id: Option<Uuid>,
    entity_label: Option<String>,
    summary: String,
    suggested_action: Option<String>,
    metadata: Value,
    created_at: time::OffsetDateTime,
    resolved_at: Option<time::OffsetDateTime>,
}

#[derive(FromRow)]
struct ExistingMutation {
    action: String,
    target_type: String,
    target_id: Uuid,
    details: Value,
}

impl PostgresEcosystemRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    fn unexpected(error: sqlx::Error) -> EcosystemRepositoryError {
        tracing::error!(error = %error, "ecosystem control-plane query failed");
        EcosystemRepositoryError::Unexpected
    }

    /// Serializes concurrent requests that share an idempotency key, so the
    /// replay check and the write cannot interleave.
    async fn lock_mutation(
        tx: &mut Transaction<'_, Postgres>,
        workspace_id: Uuid,
        idempotency_key: &str,
    ) -> Result<(), EcosystemRepositoryError> {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, hashtextextended($2, 0)))")
            .bind(workspace_id.to_string())
            .bind(idempotency_key)
            .execute(&mut **tx)
            .await
            .map_err(Self::unexpected)?;
        Ok(())
    }

    async fn existing_mutation(
        tx: &mut Transaction<'_, Postgres>,
        workspace_id: Uuid,
        idempotency_key: &str,
    ) -> Result<Option<ExistingMutation>, EcosystemRepositoryError> {
        sqlx::query_as::<_, ExistingMutation>(
            r#"
            SELECT action, target_type, target_id, details
            FROM operator_actions
            WHERE workspace_id = $1 AND idempotency_key = $2
            "#,
        )
        .bind(workspace_id)
        .bind(idempotency_key)
        .fetch_optional(&mut **tx)
        .await
        .map_err(Self::unexpected)
    }

    async fn load_flag(
        tx: &mut Transaction<'_, Postgres>,
        workspace_id: Uuid,
        key: &str,
    ) -> Result<FeatureFlagState, EcosystemRepositoryError> {
        let row = sqlx::query_as::<_, FlagRow>(
            r#"
            SELECT key, enabled, reason, version, updated_at
            FROM ecosystem_feature_flags
            WHERE workspace_id = $1 AND key = $2
            "#,
        )
        .bind(workspace_id)
        .bind(key)
        .fetch_optional(&mut **tx)
        .await
        .map_err(Self::unexpected)?
        .ok_or(EcosystemRepositoryError::UnknownFlag)?;
        Ok(FeatureFlagState {
            key: row.key,
            enabled: row.enabled,
            reason: row.reason,
            version: row.version,
            updated_at: row.updated_at,
        })
    }

    async fn resolve_event(
        tx: &mut Transaction<'_, Postgres>,
        workspace_id: Uuid,
        event_slug: &str,
    ) -> Result<Uuid, EcosystemRepositoryError> {
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM events WHERE workspace_id = $1 AND slug = $2")
            .bind(workspace_id)
            .bind(event_slug)
            .fetch_optional(&mut **tx)
            .await
            .map_err(Self::unexpected)?
            .ok_or(EcosystemRepositoryError::NotFound)
    }

    /// Read inside the mutating transaction so the returned list matches what
    /// was just committed rather than a racing writer's state.
    async fn load_checklist(
        tx: &mut Transaction<'_, Postgres>,
        workspace_id: Uuid,
        event_id: Uuid,
    ) -> Result<Vec<ShowChecklistItemState>, EcosystemRepositoryError> {
        let rows = sqlx::query_as::<_, ChecklistRow>(
            r#"
            SELECT item_key, status, note, updated_at
            FROM show_checklist_items
            WHERE workspace_id = $1 AND event_id = $2
            ORDER BY item_key
            "#,
        )
        .bind(workspace_id)
        .bind(event_id)
        .fetch_all(&mut **tx)
        .await
        .map_err(Self::unexpected)?;
        Ok(rows
            .into_iter()
            .map(|row| ShowChecklistItemState {
                item_key: row.item_key,
                status: row.status,
                note: row.note,
                updated_at: row.updated_at,
            })
            .collect())
    }

    /// Resolve findings from previous runs that this run's snapshot no longer
    /// detects. A finding is obsolete when the same `(kind, entity_id)` pair
    /// does not appear in the new run's findings — meaning the underlying
    /// condition was fixed (dead delivery retried/cancelled, ticket order
    /// corrected, etc.). Without this, findings accumulate across runs and the
    /// attention page shows duplicates that can never be cleared.
    async fn resolve_obsolete_findings(
        tx: &mut Transaction<'_, Postgres>,
        workspace_id: Uuid,
        run_id: Uuid,
    ) -> Result<(), EcosystemRepositoryError> {
        // Build the set of (kind, entity_id) pairs that this run will raise.
        // We resolve everything from prior runs first, then insert_findings
        // adds the current snapshot. Findings that persist will be re-created
        // fresh; findings that no longer apply stay resolved.
        sqlx::query(
            r#"
            UPDATE reconciliation_findings AS old
            SET resolved_at = now()
            WHERE old.workspace_id = $1
              AND old.resolved_at IS NULL
              AND old.run_id <> $2
              AND NOT EXISTS (
                  SELECT 1
                  FROM (
                      -- Mirror the insert_findings SELECT exactly so we only
                      -- keep findings that are still true in this snapshot.
                      SELECT 'ticket.pass_count_mismatch' AS kind, ticket_order.id AS entity_id
                      FROM ticket_orders AS ticket_order
                      JOIN LATERAL (
                          SELECT COALESCE(sum(item.quantity), 0)::bigint AS quantity
                          FROM ticket_order_items AS item
                          WHERE item.workspace_id = ticket_order.workspace_id
                            AND item.ticket_order_id = ticket_order.id
                      ) AS expected ON true
                      JOIN LATERAL (
                          SELECT count(pass.id)::bigint AS quantity
                          FROM admission_passes AS pass
                          JOIN ticket_order_items AS item
                            ON item.workspace_id = pass.workspace_id
                           AND item.id = pass.ticket_order_item_id
                          WHERE item.workspace_id = ticket_order.workspace_id
                            AND item.ticket_order_id = ticket_order.id
                            AND pass.issuance_method = 'paid'
                      ) AS actual ON true
                      WHERE ticket_order.workspace_id = $1
                        AND ticket_order.status IN ('paid', 'partially_refunded', 'refunded')
                        AND expected.quantity <> actual.quantity

                      UNION ALL

                      SELECT 'ticket.paid_event_missing', ticket_order.id
                      FROM ticket_orders AS ticket_order
                      WHERE ticket_order.workspace_id = $1
                        AND ticket_order.status IN ('paid', 'partially_refunded', 'refunded')
                        AND NOT EXISTS (
                            SELECT 1 FROM outbox_events AS event
                            WHERE event.workspace_id = ticket_order.workspace_id
                              AND event.event_type = 'ticket.order.paid'
                              AND event.payload ->> 'order_id' = ticket_order.id::text
                        )

                      UNION ALL

                      SELECT 'ticket.delivery_event_missing', request.ticket_order_id
                      FROM ticket_delivery_requests AS request
                      JOIN ticket_orders AS ticket_order
                        ON ticket_order.workspace_id = request.workspace_id
                       AND ticket_order.id = request.ticket_order_id
                      WHERE request.workspace_id = $1
                        AND NOT EXISTS (
                            SELECT 1 FROM outbox_events AS event
                            WHERE event.workspace_id = request.workspace_id
                              AND event.event_type = 'ticket.order.delivery_requested'
                              AND event.payload ->> 'order_id' = request.ticket_order_id::text
                              AND event.created_at >= request.created_at - interval '5 seconds'
                        )

                      UNION ALL

                      SELECT 'outbox.dead', event.id
                      FROM outbox_events AS event
                      WHERE event.workspace_id = $1 AND event.status = 'dead'

                      UNION ALL

                      SELECT 'webhook.dead', delivery.id
                      FROM webhook_deliveries AS delivery
                      JOIN webhook_endpoints AS endpoint
                        ON endpoint.workspace_id = delivery.workspace_id
                       AND endpoint.id = delivery.endpoint_id
                      WHERE delivery.workspace_id = $1 AND delivery.status = 'dead'
                  ) AS current
                  WHERE current.kind = old.kind
                    AND current.entity_id = old.entity_id
              )
            "#,
        )
        .bind(workspace_id)
        .bind(run_id)
        .execute(&mut **tx)
        .await
        .map_err(Self::unexpected)?;
        Ok(())
    }

    /// Every discrepancy class this pass knows how to detect, raised in one
    /// statement so a run is a single consistent snapshot rather than five
    /// queries racing each other.
    async fn insert_findings(
        tx: &mut Transaction<'_, Postgres>,
        workspace_id: Uuid,
        run_id: Uuid,
    ) -> Result<(), EcosystemRepositoryError> {
        sqlx::query(
            r#"
        INSERT INTO reconciliation_findings (
            workspace_id, run_id, kind, severity, entity_type, entity_id,
            entity_label, summary, suggested_action, metadata
        )
        SELECT $1, $2, 'ticket.pass_count_mismatch', 'critical', 'ticket_order',
               ticket_order.id, ticket_order.public_reference,
               'Paid ticket order does not have the expected number of admission passes',
               'inspect_ticket_order',
               jsonb_build_object('expected', expected.quantity, 'actual', actual.quantity)
        FROM ticket_orders AS ticket_order
        JOIN LATERAL (
            SELECT COALESCE(sum(item.quantity), 0)::bigint AS quantity
            FROM ticket_order_items AS item
            WHERE item.workspace_id = ticket_order.workspace_id
              AND item.ticket_order_id = ticket_order.id
        ) AS expected ON true
        JOIN LATERAL (
            SELECT count(pass.id)::bigint AS quantity
            FROM admission_passes AS pass
            JOIN ticket_order_items AS item
              ON item.workspace_id = pass.workspace_id
             AND item.id = pass.ticket_order_item_id
            WHERE item.workspace_id = ticket_order.workspace_id
              AND item.ticket_order_id = ticket_order.id
              AND pass.issuance_method = 'paid'
        ) AS actual ON true
        WHERE ticket_order.workspace_id = $1
          AND ticket_order.status IN ('paid', 'partially_refunded', 'refunded')
          AND expected.quantity <> actual.quantity

        UNION ALL

        SELECT $1, $2, 'ticket.paid_event_missing', 'warning', 'ticket_order',
               ticket_order.id, ticket_order.public_reference,
               'Paid ticket order has no durable ticket.order.paid outbox event',
               'inspect_outbox', '{}'::jsonb
        FROM ticket_orders AS ticket_order
        WHERE ticket_order.workspace_id = $1
          AND ticket_order.status IN ('paid', 'partially_refunded', 'refunded')
          AND NOT EXISTS (
              SELECT 1 FROM outbox_events AS event
              WHERE event.workspace_id = ticket_order.workspace_id
                AND event.event_type = 'ticket.order.paid'
                AND event.payload ->> 'order_id' = ticket_order.id::text
          )

        UNION ALL

        SELECT $1, $2, 'ticket.delivery_event_missing', 'warning', 'ticket_order',
               request.ticket_order_id, ticket_order.public_reference,
               'Ticket delivery request has no matching durable outbox event',
               'request_delivery_retry',
               jsonb_build_object('delivery_request_id', request.id)
        FROM ticket_delivery_requests AS request
        JOIN ticket_orders AS ticket_order
          ON ticket_order.workspace_id = request.workspace_id
         AND ticket_order.id = request.ticket_order_id
        WHERE request.workspace_id = $1
          AND NOT EXISTS (
              SELECT 1 FROM outbox_events AS event
              WHERE event.workspace_id = request.workspace_id
                AND event.event_type = 'ticket.order.delivery_requested'
                AND event.payload ->> 'order_id' = request.ticket_order_id::text
                AND event.created_at >= request.created_at - interval '5 seconds'
          )

        UNION ALL

        SELECT $1, $2, 'outbox.dead', 'critical', 'outbox_event', event.id,
               event.event_type,
               'Outbox event exhausted automatic retries', 'retry_outbox',
               jsonb_build_object('attempts', event.attempts, 'error_kind', event.last_error_kind)
        FROM outbox_events AS event
        WHERE event.workspace_id = $1 AND event.status = 'dead'

        UNION ALL

        SELECT $1, $2, 'webhook.dead', 'critical', 'webhook_delivery', delivery.id,
               endpoint.name,
               'Webhook delivery exhausted automatic retries', 'retry_delivery',
               jsonb_build_object(
                   'attempts', delivery.attempt_count,
                   'error_kind', delivery.last_error_kind,
                   'endpoint_active', endpoint.active
               )
        FROM webhook_deliveries AS delivery
        JOIN webhook_endpoints AS endpoint
          ON endpoint.workspace_id = delivery.workspace_id
         AND endpoint.id = delivery.endpoint_id
        WHERE delivery.workspace_id = $1 AND delivery.status = 'dead'
        "#,
        )
        .bind(workspace_id)
        .bind(run_id)
        .execute(&mut **tx)
        .await
        .map_err(Self::unexpected)?;
        Ok(())
    }

    async fn load_reconciliation(
        tx: &mut Transaction<'_, Postgres>,
        workspace_id: Uuid,
        run_id: Uuid,
    ) -> Result<(ReconciliationRunState, Vec<ReconciliationFindingState>), EcosystemRepositoryError>
    {
        let run = sqlx::query_as::<_, ReconciliationRunRow>(
            r#"
            SELECT id, status, trigger, finding_count, started_at, finished_at
            FROM reconciliation_runs
            WHERE workspace_id = $1 AND id = $2
            "#,
        )
        .bind(workspace_id)
        .bind(run_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(Self::unexpected)?
        .ok_or(EcosystemRepositoryError::NotFound)?;
        let findings = sqlx::query_as::<_, ReconciliationFindingRow>(
            r#"
            SELECT id, run_id, kind, severity, entity_type, entity_id,
                   entity_label, summary, suggested_action, metadata,
                   created_at, resolved_at
            FROM reconciliation_findings
            WHERE workspace_id = $1 AND run_id = $2
            ORDER BY severity DESC, created_at, id
            "#,
        )
        .bind(workspace_id)
        .bind(run_id)
        .fetch_all(&mut **tx)
        .await
        .map_err(Self::unexpected)?;
        Ok((
            ReconciliationRunState {
                id: run.id,
                status: run.status,
                trigger: run.trigger,
                finding_count: run.finding_count,
                started_at: run.started_at,
                finished_at: run.finished_at,
            },
            findings
                .into_iter()
                .map(|row| ReconciliationFindingState {
                    id: row.id,
                    run_id: row.run_id,
                    kind: row.kind,
                    severity: row.severity,
                    entity_type: row.entity_type,
                    entity_id: row.entity_id,
                    entity_label: row.entity_label,
                    summary: row.summary,
                    suggested_action: row.suggested_action,
                    metadata: row.metadata,
                    created_at: row.created_at,
                    resolved_at: row.resolved_at,
                })
                .collect(),
        ))
    }
}

/// Stable per-flag audit target, so replays of the same key compare equal.
fn deterministic_id(namespace: &str, value: &str) -> Uuid {
    let mut digest = Sha256::new();
    digest.update(namespace.as_bytes());
    digest.update([0]);
    digest.update(value.as_bytes());
    let bytes: [u8; 32] = digest.finalize().into();
    let mut id = [0_u8; 16];
    id.copy_from_slice(&bytes[..16]);
    id[6] = (id[6] & 0x0f) | 0x80;
    id[8] = (id[8] & 0x3f) | 0x80;
    Uuid::from_bytes(id)
}

fn hash_json(value: &Value) -> String {
    hex::encode(Sha256::digest(value.to_string().as_bytes()))
}

/// Identifies one auditable mutation: what happened, to what, and under which
/// payload. Shared by every control-plane command so the replay window behaves
/// the same for all of them.
#[derive(Clone, Copy)]
struct MutationIdentity<'a> {
    action: &'a str,
    target_type: &'a str,
    target_id: Uuid,
}

/// A replay is only honoured when it names the same action, the same target and
/// the same payload. Anything else reused the key for a different request.
fn validate_replay(
    existing: &ExistingMutation,
    identity: MutationIdentity<'_>,
    request_hash: &str,
) -> Result<(), EcosystemRepositoryError> {
    let existing_hash = existing.details.get("request_hash").and_then(Value::as_str);
    if existing.action == identity.action
        && existing.target_type == identity.target_type
        && existing.target_id == identity.target_id
        && existing_hash == Some(request_hash)
    {
        Ok(())
    } else {
        Err(EcosystemRepositoryError::Conflict)
    }
}

/// Records the operator action for an accepted mutation. The request hash is
/// stamped into the details so a later replay can be compared against it.
async fn append_action(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    identity: MutationIdentity<'_>,
    idempotency_key: &str,
    request_id: Option<&str>,
    request_hash: &str,
    mut details: Value,
) -> Result<(), EcosystemRepositoryError> {
    let object = details
        .as_object_mut()
        .ok_or(EcosystemRepositoryError::Unexpected)?;
    object.insert(
        "request_hash".to_owned(),
        Value::String(request_hash.to_owned()),
    );
    sqlx::query(
        r#"
        INSERT INTO operator_actions (
            workspace_id, action, target_type, target_id, actor_type,
            idempotency_key, request_id, details
        ) VALUES ($1, $2, $3, $4, 'admin_api_key', $5, $6, $7)
        "#,
    )
    .bind(workspace_id)
    .bind(identity.action)
    .bind(identity.target_type)
    .bind(identity.target_id)
    .bind(idempotency_key)
    .bind(request_id)
    .bind(&details)
    .execute(&mut **tx)
    .await
    .map_err(PostgresEcosystemRepository::unexpected)?;
    Ok(())
}

#[async_trait]
impl EcosystemControlPlaneRepository for PostgresEcosystemRepository {
    async fn update_feature_flag(
        &self,
        command: &UpdateFeatureFlagCommand,
    ) -> Result<FeatureFlagMutation, EcosystemRepositoryError> {
        let workspace_id = command.workspace_id.into_uuid();
        let reason = command
            .reason
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let request_hash = hash_json(&json!({
            "key": command.key,
            "enabled": command.enabled,
            "reason": command.reason,
            "expected_version": command.expected_version,
        }));
        let identity = MutationIdentity {
            action: FLAG_ACTION,
            target_type: FLAG_TARGET_TYPE,
            target_id: deterministic_id(FLAG_TARGET_TYPE, &command.key),
        };

        let mut tx = self.pool.begin().await.map_err(Self::unexpected)?;
        Self::lock_mutation(&mut tx, workspace_id, &command.idempotency_key).await?;

        if let Some(existing) =
            Self::existing_mutation(&mut tx, workspace_id, &command.idempotency_key).await?
        {
            validate_replay(&existing, identity, &request_hash)?;
            let flag = Self::load_flag(&mut tx, workspace_id, &command.key).await?;
            tx.commit().await.map_err(Self::unexpected)?;
            return Ok(FeatureFlagMutation {
                flag,
                replayed: true,
            });
        }

        let update = sqlx::query(
            r#"
            INSERT INTO ecosystem_feature_flags (
                workspace_id, key, enabled, reason, updated_by_request_id
            )
            SELECT $1, $2, $3, $4, $5
            WHERE $6::bigint IS NULL OR EXISTS (
                SELECT 1
                FROM ecosystem_feature_flags AS current_flag
                WHERE current_flag.workspace_id = $1
                  AND current_flag.key = $2
                  AND current_flag.version = $6
            )
            ON CONFLICT (workspace_id, key) DO UPDATE
            SET enabled = EXCLUDED.enabled,
                reason = EXCLUDED.reason,
                version = ecosystem_feature_flags.version + 1,
                updated_at = now(),
                updated_by_request_id = EXCLUDED.updated_by_request_id
            WHERE $6::bigint IS NULL OR ecosystem_feature_flags.version = $6
            "#,
        )
        .bind(workspace_id)
        .bind(&command.key)
        .bind(command.enabled)
        .bind(reason)
        .bind(command.request_id.as_deref())
        .bind(command.expected_version)
        .execute(&mut *tx)
        .await
        .map_err(Self::unexpected)?;

        if update.rows_affected() == 0 {
            return Err(EcosystemRepositoryError::Conflict);
        }

        append_action(
            &mut tx,
            workspace_id,
            identity,
            &command.idempotency_key,
            command.request_id.as_deref(),
            &request_hash,
            json!({"key": command.key, "enabled": command.enabled}),
        )
        .await?;

        let flag = Self::load_flag(&mut tx, workspace_id, &command.key).await?;
        tx.commit().await.map_err(Self::unexpected)?;
        Ok(FeatureFlagMutation {
            flag,
            replayed: false,
        })
    }

    async fn update_show_checklist(
        &self,
        command: &UpdateShowChecklistCommand,
    ) -> Result<ShowChecklistMutation, EcosystemRepositoryError> {
        let workspace_id = command.workspace_id.into_uuid();
        let note = command
            .note
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let request_hash = hash_json(&json!({
            "event_slug": command.event_slug,
            "item_key": command.item_key,
            "status": command.status,
            "note": command.note,
        }));

        let mut tx = self.pool.begin().await.map_err(Self::unexpected)?;
        // Resolving the event inside the transaction keeps the audit target and
        // the write addressing the same row.
        let event_id = Self::resolve_event(&mut tx, workspace_id, &command.event_slug).await?;
        let identity = MutationIdentity {
            action: CHECKLIST_ACTION,
            target_type: CHECKLIST_TARGET_TYPE,
            target_id: deterministic_id("checklist", &format!("{event_id}:{}", command.item_key)),
        };
        Self::lock_mutation(&mut tx, workspace_id, &command.idempotency_key).await?;

        if let Some(existing) =
            Self::existing_mutation(&mut tx, workspace_id, &command.idempotency_key).await?
        {
            validate_replay(&existing, identity, &request_hash)?;
            let items = Self::load_checklist(&mut tx, workspace_id, event_id).await?;
            tx.commit().await.map_err(Self::unexpected)?;
            return Ok(ShowChecklistMutation {
                event_id,
                items,
                replayed: true,
            });
        }

        sqlx::query(
            r#"
            INSERT INTO show_checklist_items (
                workspace_id, event_id, item_key, status, note, updated_by_request_id
            ) VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (workspace_id, event_id, item_key) DO UPDATE
            SET status = EXCLUDED.status,
                note = EXCLUDED.note,
                updated_at = now(),
                updated_by_request_id = EXCLUDED.updated_by_request_id
            "#,
        )
        .bind(workspace_id)
        .bind(event_id)
        .bind(&command.item_key)
        .bind(&command.status)
        .bind(note)
        .bind(command.request_id.as_deref())
        .execute(&mut *tx)
        .await
        .map_err(Self::unexpected)?;

        append_action(
            &mut tx,
            workspace_id,
            identity,
            &command.idempotency_key,
            command.request_id.as_deref(),
            &request_hash,
            json!({"event_id": event_id, "item_key": command.item_key}),
        )
        .await?;

        let items = Self::load_checklist(&mut tx, workspace_id, event_id).await?;
        tx.commit().await.map_err(Self::unexpected)?;
        Ok(ShowChecklistMutation {
            event_id,
            items,
            replayed: false,
        })
    }

    async fn run_reconciliation(
        &self,
        command: &RunReconciliationCommand,
    ) -> Result<ReconciliationOutcome, EcosystemRepositoryError> {
        let workspace_id = command.workspace_id.into_uuid();
        let request_hash = hash_json(&json!({"trigger": command.trigger}));
        let identity = MutationIdentity {
            action: RECONCILE_ACTION,
            target_type: RECONCILE_TARGET_TYPE,
            target_id: deterministic_id("reconciliation", &command.idempotency_key),
        };

        let mut tx = self.pool.begin().await.map_err(Self::unexpected)?;
        Self::lock_mutation(&mut tx, workspace_id, &command.idempotency_key).await?;

        if let Some(existing) =
            Self::existing_mutation(&mut tx, workspace_id, &command.idempotency_key).await?
        {
            validate_replay(&existing, identity, &request_hash)?;
            // The stored action carries the run this key already produced.
            // Without it the replay would have no run to return, which is a
            // corrupted audit row rather than a fresh request.
            let run_id = existing
                .details
                .get("run_id")
                .and_then(Value::as_str)
                .and_then(|value| Uuid::parse_str(value).ok())
                .ok_or(EcosystemRepositoryError::Conflict)?;
            let (run, findings) = Self::load_reconciliation(&mut tx, workspace_id, run_id).await?;
            tx.commit().await.map_err(Self::unexpected)?;
            return Ok(ReconciliationOutcome {
                run,
                findings,
                replayed: true,
            });
        }

        let run_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO reconciliation_runs (id, workspace_id, status, trigger, request_id)
            VALUES ($1, $2, 'running', $3, $4)
            "#,
        )
        .bind(run_id)
        .bind(workspace_id)
        .bind(&command.trigger)
        .bind(command.request_id.as_deref())
        .execute(&mut *tx)
        .await
        .map_err(Self::unexpected)?;

        // Resolve findings from previous runs that are no longer present in
        // this run's snapshot. Without this, findings accumulate across runs —
        // a dead delivery that was cancelled still shows as an open finding
        // from every prior run that observed it.
        Self::resolve_obsolete_findings(&mut tx, workspace_id, run_id).await?;

        Self::insert_findings(&mut tx, workspace_id, run_id).await?;

        let finding_count = sqlx::query_scalar::<_, i64>(
            "SELECT count(*)::bigint FROM reconciliation_findings WHERE workspace_id = $1 AND run_id = $2",
        )
        .bind(workspace_id)
        .bind(run_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(Self::unexpected)?;
        let finding_count =
            i32::try_from(finding_count).map_err(|_| EcosystemRepositoryError::Unexpected)?;

        sqlx::query(
            r#"
            UPDATE reconciliation_runs
            SET status = 'completed', finding_count = $3, finished_at = now()
            WHERE workspace_id = $1 AND id = $2
            "#,
        )
        .bind(workspace_id)
        .bind(run_id)
        .bind(finding_count)
        .execute(&mut *tx)
        .await
        .map_err(Self::unexpected)?;

        sqlx::query(
            r#"
INSERT INTO outbox_events (workspace_id, event_type, event_version, payload, request_id)
        SELECT finding.workspace_id,
               'reconciliation.finding_raised',
               1,
               jsonb_build_object(
                   'finding_id', finding.id,
                   'run_id', finding.run_id,
                   'kind', finding.kind,
                   'severity', finding.severity,
                   'entity_id', finding.entity_id,
                   'entity_label', finding.entity_label,
                   'summary', finding.summary,
                   'suggested_action', finding.suggested_action
               ),
               $3
        FROM reconciliation_findings AS finding
        WHERE finding.workspace_id = $1 AND finding.run_id = $2
          AND finding.severity IN ('warning', 'critical')
            "#,
        )
        .bind(workspace_id)
        .bind(run_id)
        .bind(command.request_id.as_deref())
        .execute(&mut *tx)
        .await
        .map_err(Self::unexpected)?;

        append_action(
            &mut tx,
            workspace_id,
            identity,
            &command.idempotency_key,
            command.request_id.as_deref(),
            &request_hash,
            json!({
                "run_id": run_id,
                "finding_count": finding_count,
                "trigger": command.trigger,
            }),
        )
        .await?;

        let (run, findings) = Self::load_reconciliation(&mut tx, workspace_id, run_id).await?;
        tx.commit().await.map_err(Self::unexpected)?;
        Ok(ReconciliationOutcome {
            run,
            findings,
            replayed: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{deterministic_id, hash_json};
    use serde_json::json;

    /// Audit targets are derived, not stored, so a replay of the same key must
    /// land on the same row and a different namespace must never collide with it.
    #[test]
    fn deterministic_ids_are_namespaced_and_uuid_v8() {
        let first = deterministic_id("feature_flag", "ticket_sales_enabled");
        assert_eq!(
            first,
            deterministic_id("feature_flag", "ticket_sales_enabled")
        );
        assert_ne!(first, deterministic_id("checklist", "ticket_sales_enabled"));
        assert_ne!(
            first,
            deterministic_id("reconciliation", "ticket_sales_enabled")
        );
        assert_eq!(first.get_version_num(), 8);
    }

    /// The replay check compares stored against recomputed hashes, so equal
    /// payloads must hash equally and any difference must be visible.
    #[test]
    fn request_hashes_are_stable_and_sensitive() {
        assert_eq!(hash_json(&json!({"a": 1})), hash_json(&json!({"a": 1})));
        assert_ne!(hash_json(&json!({"a": 1})), hash_json(&json!({"a": 2})));
    }
}
