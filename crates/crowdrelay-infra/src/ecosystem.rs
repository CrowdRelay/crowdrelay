//! PostgreSQL ecosystem control-plane repository.
//!
//! Owns the flag row, the advisory lock that serializes a replay window, and
//! the `operator_actions` audit row. All three commit together: an accepted
//! flag flip is always auditable, and a replay never writes a second time.

use async_trait::async_trait;
use crowdrelay_application::{
    EcosystemControlPlaneRepository, EcosystemRepositoryError, FeatureFlagMutation,
    FeatureFlagState, ShowChecklistItemState, ShowChecklistMutation, UpdateFeatureFlagCommand,
    UpdateShowChecklistCommand,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use uuid::Uuid;

const FLAG_ACTION: &str = "feature_flag.updated";
const FLAG_TARGET_TYPE: &str = "feature_flag";
const CHECKLIST_ACTION: &str = "show_checklist.updated";
const CHECKLIST_TARGET_TYPE: &str = "show_checklist";

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
            workspace_id, action, target_type, target_id,
            idempotency_key, request_id, details
        ) VALUES ($1, $2, $3, $4, $5, $6, $7)
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

        sqlx::query(
            r#"
            INSERT INTO ecosystem_feature_flags (
                workspace_id, key, enabled, reason, updated_by_request_id
            ) VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (workspace_id, key) DO UPDATE
            SET enabled = EXCLUDED.enabled,
                reason = EXCLUDED.reason,
                version = ecosystem_feature_flags.version + 1,
                updated_at = now(),
                updated_by_request_id = EXCLUDED.updated_by_request_id
            "#,
        )
        .bind(workspace_id)
        .bind(&command.key)
        .bind(command.enabled)
        .bind(reason)
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
}
