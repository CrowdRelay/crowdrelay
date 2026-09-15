//! PostgreSQL implementation of the fan identity spine (§4e-5).
//!
//! The merge is one transaction: every movable `fan_id` table re-points to
//! the survivor under a `NOT EXISTS` guard so unique constraints cannot
//! collide, append-only tables are never touched, the merged fan is
//! tombstoned (`status='merged'`, `merged_into_fan_id`), and a `fan_merges`
//! row records exactly which row keys moved. Unmerge replays that record.

use async_trait::async_trait;
use crowdrelay_application::{
    DismissMergeCandidateCommand, FanIdentifierView, FanIdentityError, FanIdentityRepository,
    FanMergeView, MergeCandidateView, MergeFansCommand, UnmergeFanCommand,
};
use crowdrelay_domain::{
    WorkspaceId,
    fan_identity::{CandidateStatus, MergeError, canonical_pair, validate_merge},
};
use serde_json::{Map, Value, json};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

/// PostgreSQL implementation of [`FanIdentityRepository`].
#[derive(Clone)]
pub struct PgFanIdentityRepository {
    pool: PgPool,
}

impl PgFanIdentityRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// One movable table's merge policy.
struct MoveSpec {
    /// Audit key in `moved`/`retained` — the table name.
    table: &'static str,
    /// UPDATE that re-points rows; returns the audit-key column as text.
    /// Params: $1 workspace, $2 survivor, $3 merged.
    move_sql: &'static str,
}

/// Per-table move policy. Tables absent here are never re-pointed:
/// `fan_consents`, `fan_acquisition_events`, `referral_attributions`,
/// `operator_actions`, `reward_draw_proofs`, `viryaos_action_ledger`,
/// `audit_events`, `external_proof_items` are append-only or pinned
/// history — they keep describing the record they were written against.
/// `fan_provenance_events` has no mutation trigger and describes the
/// person, so it moves with them.
const MOVE_SPECS: &[MoveSpec] = &[
    // --- free moves: no fan-scoped unique constraint can collide ---
    MoveSpec {
        table: "ad_conversion_deliveries",
        move_sql: "UPDATE ad_conversion_deliveries SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 RETURNING id::text",
    },
    MoveSpec {
        table: "amplification_deliveries",
        move_sql: "UPDATE amplification_deliveries t SET fan_id = $2 WHERE to_workspace_id = $1 AND fan_id = $3 \
         AND NOT EXISTS (SELECT 1 FROM amplification_deliveries s \
             WHERE s.consent_id = t.consent_id AND s.fan_id = $2 \
             AND s.campaign_reference = t.campaign_reference) RETURNING id::text",
    },
    MoveSpec {
        table: "fan_provenance_events",
        move_sql: "UPDATE fan_provenance_events SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 RETURNING id::text",
    },
    MoveSpec {
        table: "fan_push_deliveries",
        move_sql: "UPDATE fan_push_deliveries SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 RETURNING id::text",
    },
    MoveSpec {
        table: "fan_push_endpoints",
        move_sql: "UPDATE fan_push_endpoints SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 RETURNING id::text",
    },
    MoveSpec {
        table: "fan_sessions",
        move_sql: "UPDATE fan_sessions SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 RETURNING id::text",
    },
    MoveSpec {
        table: "fanbase_members",
        move_sql: "UPDATE fanbase_members SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 RETURNING id::text",
    },
    MoveSpec {
        table: "merch_order_facts",
        move_sql: "UPDATE merch_order_facts SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 RETURNING id::text",
    },
    MoveSpec {
        table: "signal_installations",
        move_sql: "UPDATE signal_installations SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 RETURNING installation_id::text",
    },
    MoveSpec {
        table: "synesthesia_reward_entries",
        move_sql: "UPDATE synesthesia_reward_entries SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 RETURNING id::text",
    },
    MoveSpec {
        table: "synesthesia_runs",
        move_sql: "UPDATE synesthesia_runs SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 RETURNING id::text",
    },
    // --- guarded moves: a survivor row holding the same business key wins;
    //     the loser's row stays on the merged fan and is recorded retained ---
    MoveSpec {
        table: "admission_passes",
        move_sql: "UPDATE admission_passes t SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 \
         AND NOT EXISTS (SELECT 1 FROM admission_passes s WHERE s.workspace_id = $1 \
             AND s.admission_pool_id = t.admission_pool_id AND s.fan_id = $2) \
         RETURNING id::text",
    },
    MoveSpec {
        table: "area_players",
        move_sql: "UPDATE area_players SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 \
         AND NOT EXISTS (SELECT 1 FROM area_players s WHERE s.workspace_id = $1 AND s.fan_id = $2) \
         RETURNING id::text",
    },
    // Recipients move before deliveries: a delivery holds a composite FK
    // to its recipient (workspace_id, campaign_id, fan_id), so the parent's
    // row must already name the survivor when the delivery re-points.
    MoveSpec {
        table: "communication_campaign_recipients",
        move_sql: "UPDATE communication_campaign_recipients t SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 \
         AND NOT EXISTS (SELECT 1 FROM communication_campaign_recipients s \
             WHERE s.workspace_id = $1 AND s.campaign_id = t.campaign_id AND s.fan_id = $2) \
         RETURNING campaign_id::text",
    },
    MoveSpec {
        table: "communication_campaign_deliveries",
        move_sql: "UPDATE communication_campaign_deliveries t SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 \
         AND NOT EXISTS (SELECT 1 FROM communication_campaign_deliveries s \
             WHERE s.workspace_id = $1 AND s.campaign_id = t.campaign_id AND s.fan_id = $2) \
         RETURNING campaign_id::text",
    },
    MoveSpec {
        table: "concert_checkins",
        move_sql: "UPDATE concert_checkins t SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 \
         AND NOT EXISTS (SELECT 1 FROM concert_checkins s \
             WHERE s.workspace_id = $1 AND s.event_id = t.event_id AND s.fan_id = $2) \
         RETURNING id::text",
    },
    MoveSpec {
        table: "event_interests",
        move_sql: "UPDATE event_interests t SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 \
         AND NOT EXISTS (SELECT 1 FROM event_interests s \
             WHERE s.workspace_id = $1 AND s.event_id = t.event_id AND s.fan_id = $2) \
         RETURNING event_id::text",
    },
    MoveSpec {
        table: "event_reminder_jobs",
        move_sql: "UPDATE event_reminder_jobs t SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 \
         AND NOT EXISTS (SELECT 1 FROM event_reminder_jobs s \
             WHERE s.workspace_id = $1 AND s.event_id = t.event_id AND s.fan_id = $2 \
             AND s.reminder_kind = t.reminder_kind) RETURNING id::text",
    },
    MoveSpec {
        table: "fan_action_tokens",
        move_sql: "UPDATE fan_action_tokens t SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 \
         AND (t.consumed_at IS NOT NULL OR NOT EXISTS ( \
             SELECT 1 FROM fan_action_tokens s WHERE s.workspace_id = $1 AND s.fan_id = $2 \
             AND s.purpose = t.purpose AND s.consumed_at IS NULL)) RETURNING id::text",
    },
    MoveSpec {
        table: "fan_ad_attribution",
        move_sql: "UPDATE fan_ad_attribution SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 \
         AND NOT EXISTS (SELECT 1 FROM fan_ad_attribution s WHERE s.workspace_id = $1 AND s.fan_id = $2) \
         RETURNING fan_id::text",
    },
    MoveSpec {
        table: "fan_audience_tags",
        move_sql: "UPDATE fan_audience_tags t SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 \
         AND NOT EXISTS (SELECT 1 FROM fan_audience_tags s \
             WHERE s.workspace_id = $1 AND s.fan_id = $2 AND s.tag = t.tag) RETURNING tag",
    },
    MoveSpec {
        table: "fan_city_interests",
        move_sql: "UPDATE fan_city_interests t SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 \
         AND NOT EXISTS (SELECT 1 FROM fan_city_interests s \
             WHERE s.workspace_id = $1 AND s.fan_id = $2 AND s.city_id = t.city_id) \
         RETURNING city_id::text",
    },
    MoveSpec {
        table: "fan_location_preferences",
        move_sql: "UPDATE fan_location_preferences SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 \
         AND NOT EXISTS (SELECT 1 FROM fan_location_preferences s WHERE s.workspace_id = $1 AND s.fan_id = $2) \
         RETURNING fan_id::text",
    },
    MoveSpec {
        table: "fan_push_preferences",
        move_sql: "UPDATE fan_push_preferences SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 \
         AND NOT EXISTS (SELECT 1 FROM fan_push_preferences s WHERE s.workspace_id = $1 AND s.fan_id = $2) \
         RETURNING fan_id::text",
    },
    MoveSpec {
        table: "nearby_gig_notifications",
        move_sql: "UPDATE nearby_gig_notifications t SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 \
         AND NOT EXISTS (SELECT 1 FROM nearby_gig_notifications s \
             WHERE s.workspace_id = $1 AND s.fan_id = $2 AND s.event_id = t.event_id) \
         RETURNING event_id::text",
    },
    // A code referenced by attributions or acquisition events can never
    // move — those tables pin (workspace_id, code_id, fan_id) with a
    // composite FK. It stays on the tombstone, recorded as retained.
    MoveSpec {
        table: "referral_codes",
        move_sql: "UPDATE referral_codes t SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 \
         AND NOT EXISTS (SELECT 1 FROM referral_attributions ra \
             WHERE ra.workspace_id = $1 AND ra.referral_code_id = t.id) \
         AND NOT EXISTS (SELECT 1 FROM fan_acquisition_events e \
             WHERE e.workspace_id = $1 AND e.referral_code_id = t.id) \
         AND (NOT t.active OR NOT EXISTS ( \
             SELECT 1 FROM referral_codes s WHERE s.workspace_id = $1 AND s.fan_id = $2 AND s.active)) \
         RETURNING id::text",
    },
    MoveSpec {
        table: "reward_draw_candidates",
        move_sql: "UPDATE reward_draw_candidates t SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 \
         AND NOT EXISTS (SELECT 1 FROM reward_draw_candidates s \
             WHERE s.workspace_id = $1 AND s.fan_id = $2 AND s.run_id = t.run_id) \
         RETURNING run_id::text",
    },
    MoveSpec {
        table: "reward_draw_winners",
        move_sql: "UPDATE reward_draw_winners t SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 \
         AND NOT EXISTS (SELECT 1 FROM reward_draw_winners s \
             WHERE s.workspace_id = $1 AND s.fan_id = $2 AND s.draw_id = t.draw_id) \
         RETURNING id::text",
    },
    MoveSpec {
        table: "reward_grants",
        move_sql: "UPDATE reward_grants t SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 \
         AND NOT EXISTS (SELECT 1 FROM reward_grants s \
             WHERE s.workspace_id = $1 AND s.fan_id = $2 AND s.reward_rule_id = t.reward_rule_id \
             AND s.qualification_key = t.qualification_key) RETURNING id::text",
    },
    MoveSpec {
        table: "viryaos_play_step_recipients",
        move_sql: "UPDATE viryaos_play_step_recipients t SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 \
         AND NOT EXISTS (SELECT 1 FROM viryaos_play_step_recipients s \
             WHERE s.workspace_id = $1 AND s.fan_id = $2 AND s.step_id = t.step_id) \
         RETURNING id::text",
    },
    MoveSpec {
        table: "viryaos_reach_conversions",
        move_sql: "UPDATE viryaos_reach_conversions t SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 \
         AND NOT EXISTS (SELECT 1 FROM viryaos_reach_conversions s \
             WHERE s.reach_event_id = t.reach_event_id AND s.fan_id = $2) RETURNING id::text",
    },
];

/// Re-point one table's rows to the survivor and return the moved keys.
async fn move_rows(
    tx: &mut Transaction<'_, Postgres>,
    spec: &MoveSpec,
    workspace_id: Uuid,
    survivor: Uuid,
    merged: Uuid,
) -> Result<Vec<String>, FanIdentityError> {
    sqlx::query_scalar::<_, String>(spec.move_sql)
        .bind(workspace_id)
        .bind(survivor)
        .bind(merged)
        .fetch_all(&mut **tx)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "fan identity store failed");
            FanIdentityError::Unavailable
        })
}

#[derive(Debug, FromRow)]
struct FanStateRow {
    id: Uuid,
    status: String,
}

#[derive(Debug, FromRow)]
struct CandidateRow {
    id: Uuid,
    fan_id_a: Uuid,
    fan_id_b: Uuid,
    evidence: Value,
    created_at: OffsetDateTime,
}

#[derive(Debug, FromRow)]
struct MergeRow {
    id: Uuid,
    survivor_fan_id: Uuid,
    merged_fan_id: Uuid,
    moved: Value,
    retained: Value,
    consents_mirrored: i32,
    merged_at: OffsetDateTime,
    unmerged_at: Option<OffsetDateTime>,
}

#[derive(Debug, FromRow)]
struct IdentifierRow {
    kind: String,
    value: String,
    source: String,
    verified_at: OffsetDateTime,
}

fn fmt_time(t: OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| t.to_string())
}

impl CandidateRow {
    fn into_view(self) -> MergeCandidateView {
        let evidence_kind = self
            .evidence
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        MergeCandidateView {
            id: self.id,
            fan_id_a: self.fan_id_a,
            fan_id_b: self.fan_id_b,
            evidence_kind,
            evidence: self.evidence,
            created_at: fmt_time(self.created_at),
        }
    }
}

impl MergeRow {
    fn into_view(self) -> FanMergeView {
        FanMergeView {
            id: self.id,
            survivor_fan_id: self.survivor_fan_id,
            merged_fan_id: self.merged_fan_id,
            moved_counts: count_map(&self.moved),
            retained_counts: count_map(&self.retained),
            consents_mirrored: self.consents_mirrored,
            merged_at: fmt_time(self.merged_at),
            unmerged_at: self.unmerged_at.map(fmt_time),
        }
    }
}

/// `moved` stores {"table": ["k1","k2"]} (the exact keys unmerge replays)
/// while `retained` stores {"table": n} (counts only — retained rows stay
/// put, so there is nothing to replay). The API view shows counts for both.
fn count_map(value: &Value) -> Value {
    let mut out = Map::new();
    if let Some(obj) = value.as_object() {
        for (table, entry) in obj {
            let count = entry
                .as_array()
                .map_or_else(|| entry.as_i64().unwrap_or(0), |a| a.len() as i64);
            out.insert(table.clone(), json!(count));
        }
    }
    Value::Object(out)
}

#[async_trait]
impl FanIdentityRepository for PgFanIdentityRepository {
    async fn list_merge_candidates(
        &self,
        workspace_id: WorkspaceId,
        status: Option<CandidateStatus>,
        limit: u32,
    ) -> Result<Vec<MergeCandidateView>, FanIdentityError> {
        let rows = sqlx::query_as::<_, CandidateRow>(
            "SELECT id, fan_id_a, fan_id_b, evidence, created_at \
             FROM fan_merge_candidates \
             WHERE workspace_id = $1 AND status = $2 \
             ORDER BY created_at DESC LIMIT $3",
        )
        .bind(workspace_id.into_uuid())
        .bind(status.unwrap_or(CandidateStatus::Pending).as_str())
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "fan identity store failed");
            FanIdentityError::Unavailable
        })?;
        Ok(rows.into_iter().map(CandidateRow::into_view).collect())
    }

    async fn list_fan_identifiers(
        &self,
        workspace_id: WorkspaceId,
        fan_id: Uuid,
    ) -> Result<Vec<FanIdentifierView>, FanIdentityError> {
        let rows = sqlx::query_as::<_, IdentifierRow>(
            "SELECT kind, value, source, verified_at FROM fan_identifiers \
             WHERE workspace_id = $1 AND fan_id = $2 ORDER BY verified_at",
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "fan identity store failed");
            FanIdentityError::Unavailable
        })?;
        Ok(rows
            .into_iter()
            .map(|r| FanIdentifierView {
                kind: r.kind,
                value: r.value,
                source: r.source,
                verified_at: fmt_time(r.verified_at),
            })
            .collect())
    }

    async fn merge_fans(
        &self,
        command: &MergeFansCommand,
    ) -> Result<FanMergeView, FanIdentityError> {
        let workspace_id = command.workspace_id.into_uuid();
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|_| FanIdentityError::Unavailable)?;

        // A delivery's recipient FK is deferrable: the merge re-points both
        // tables, and no statement order satisfies an immediate check —
        // defer it to commit when the pair has settled.
        sqlx::query("SET CONSTRAINTS communication_campaign_deliveries_recipient_fk DEFERRED")
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                tracing::warn!(%error, "fan identity store failed");
                FanIdentityError::Unavailable
            })?;

        // Lock both fan rows in canonical id order so two opposite-direction
        // merges can't deadlock.
        let (first, second) = canonical_pair(command.survivor_fan_id, command.merged_fan_id);
        let rows = sqlx::query_as::<_, FanStateRow>(
            "SELECT id, status FROM fans WHERE workspace_id = $1 AND id IN ($2, $3) \
             ORDER BY id FOR UPDATE",
        )
        .bind(workspace_id)
        .bind(first)
        .bind(second)
        .fetch_all(&mut *tx)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "fan identity store failed");
            FanIdentityError::Unavailable
        })?;
        let survivor = rows
            .iter()
            .find(|r| r.id == command.survivor_fan_id)
            .ok_or(FanIdentityError::NotFound)?;
        let merged = rows
            .iter()
            .find(|r| r.id == command.merged_fan_id)
            .ok_or(FanIdentityError::NotFound)?;
        validate_merge(
            command.survivor_fan_id,
            &survivor.status,
            command.merged_fan_id,
            &merged.status,
        )?;

        // Chain merges are forbidden: if the merged fan is itself the
        // survivor of an open merge, an out-of-order unmerge could never
        // replay its rows (they would live on the newer survivor). Merge
        // the newer identity into the root survivor instead.
        let merged_is_survivor = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM fan_merges WHERE workspace_id = $1 \
             AND survivor_fan_id = $2 AND unmerged_at IS NULL)",
        )
        .bind(workspace_id)
        .bind(command.merged_fan_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "fan identity store failed");
            FanIdentityError::Unavailable
        })?;
        if merged_is_survivor {
            return Err(MergeError::MergedFanIsSurvivor.into());
        }

        let mut moved: Map<String, Value> = Map::new();
        for spec in MOVE_SPECS {
            let keys = move_rows(
                &mut tx,
                spec,
                workspace_id,
                command.survivor_fan_id,
                command.merged_fan_id,
            )
            .await?;
            if !keys.is_empty() {
                moved.insert(spec.table.to_string(), json!(keys));
            }
        }

        // Identifiers all move — the (kind, value) unique can't collide and
        // post-merge resolution must route the old addresses to the survivor.
        let identifier_keys = sqlx::query_scalar::<_, String>(
            "UPDATE fan_identifiers SET fan_id = $2 WHERE workspace_id = $1 AND fan_id = $3 \
             RETURNING id::text",
        )
        .bind(workspace_id)
        .bind(command.survivor_fan_id)
        .bind(command.merged_fan_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "fan identity store failed");
            FanIdentityError::Unavailable
        })?;
        if !identifier_keys.is_empty() {
            moved.insert("fan_identifiers".to_string(), json!(identifier_keys));
        }

        // Mirror the loser's latest consent decision for any purpose the
        // survivor has never decided. The survivor's own word always wins —
        // a merge must never silently grant or revoke contact permission.
        let consents_mirrored = sqlx::query(
            "WITH latest AS ( \
                 SELECT DISTINCT ON (purpose) purpose, granted, policy_version, source, recorded_at \
                 FROM fan_consents WHERE workspace_id = $1 AND fan_id = $3 \
                 ORDER BY purpose, recorded_at DESC \
             ), missing AS ( \
                 SELECT l.* FROM latest l WHERE NOT EXISTS ( \
                     SELECT 1 FROM fan_consents s WHERE s.workspace_id = $1 \
                     AND s.fan_id = $2 AND s.purpose = l.purpose)) \
             INSERT INTO fan_consents (workspace_id, fan_id, purpose, granted, policy_version, source, recorded_at) \
             SELECT $1, $2, purpose, granted, policy_version, 'fan_merge', now() FROM missing",
        )
        .bind(workspace_id)
        .bind(command.survivor_fan_id)
        .bind(command.merged_fan_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "fan identity store failed");
            FanIdentityError::Unavailable
        })?
        .rows_affected() as i64;

        // Retained = whatever rows still name the merged fan afterwards.
        let mut retained: Map<String, Value> = Map::new();
        for spec in MOVE_SPECS {
            // amplification_deliveries is the cross-workspace table — its
            // tenant column is to_workspace_id.
            let ws_col = if spec.table == "amplification_deliveries" {
                "to_workspace_id"
            } else {
                "workspace_id"
            };
            let remaining = sqlx::query_scalar::<_, i64>(&format!(
                "SELECT count(*) FROM {} WHERE {ws_col} = $1 AND fan_id = $2",
                spec.table
            ))
            .bind(workspace_id)
            .bind(command.merged_fan_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|error| {
                tracing::warn!(%error, "fan identity store failed");
                FanIdentityError::Unavailable
            })?;
            if remaining > 0 {
                retained.insert(spec.table.to_string(), json!(remaining));
            }
        }
        // Append-only and pinned history always stays behind — count it so
        // the audit is honest about what was deliberately left on the
        // tombstone. The second set keys fans by non-`fan_id` columns.
        for table in ["fan_consents", "fan_acquisition_events"] {
            let remaining = sqlx::query_scalar::<_, i64>(&format!(
                "SELECT count(*) FROM {table} WHERE workspace_id = $1 AND fan_id = $2"
            ))
            .bind(workspace_id)
            .bind(command.merged_fan_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|error| {
                tracing::warn!(%error, "fan identity store failed");
                FanIdentityError::Unavailable
            })?;
            if remaining > 0 {
                retained.insert(table.to_string(), json!(remaining));
            }
        }
        for (table, column) in [
            ("referral_attributions", "referrer_fan_id"),
            ("referral_attributions", "referred_fan_id"),
            ("viryaos_growth_evidence", "converted_fan_id"),
            ("viryaos_reach_events", "converted_fan_id"),
        ] {
            let remaining = sqlx::query_scalar::<_, i64>(&format!(
                "SELECT count(*) FROM {table} WHERE workspace_id = $1 AND {column} = $2"
            ))
            .bind(workspace_id)
            .bind(command.merged_fan_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|error| {
                tracing::warn!(%error, "fan identity store failed");
                FanIdentityError::Unavailable
            })?;
            if remaining > 0 {
                let prior = retained.get(table).and_then(Value::as_i64).unwrap_or(0);
                retained.insert(table.to_string(), json!(prior + remaining));
            }
        }

        // Tombstone the merged fan. prior_status is recorded on the merge row
        // so unmerge restores exactly what was there.
        sqlx::query(
            "UPDATE fans SET status = 'merged', merged_into_fan_id = $2, merged_at = now(), \
             updated_at = now() WHERE workspace_id = $1 AND id = $3",
        )
        .bind(workspace_id)
        .bind(command.survivor_fan_id)
        .bind(command.merged_fan_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "fan identity store failed");
            FanIdentityError::Unavailable
        })?;

        let merge = sqlx::query_as::<_, MergeRow>(
            "INSERT INTO fan_merges (workspace_id, survivor_fan_id, merged_fan_id, prior_status, \
                 reason, moved, retained, consents_mirrored, merged_by, merged_at, request_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, now(), $10) \
             RETURNING id, survivor_fan_id, merged_fan_id, moved, retained, consents_mirrored, \
                 merged_at, unmerged_at",
        )
        .bind(workspace_id)
        .bind(command.survivor_fan_id)
        .bind(command.merged_fan_id)
        .bind(&merged.status)
        .bind(command.reason.as_deref())
        .bind(Value::Object(moved))
        .bind(Value::Object(retained))
        .bind(i32::try_from(consents_mirrored).unwrap_or(i32::MAX))
        .bind(command.merged_by.as_str())
        .bind(command.request_id.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "fan identity store failed");
            FanIdentityError::Unavailable
        })?;

        // Resolve every open candidate for this pair — the human decided.
        let (fan_a, fan_b) = canonical_pair(command.survivor_fan_id, command.merged_fan_id);
        sqlx::query(
            "UPDATE fan_merge_candidates SET status = 'merged', resolved_at = now(), \
             resolved_merge_id = $4 WHERE workspace_id = $1 AND fan_id_a = $2 AND fan_id_b = $3 \
             AND status = 'pending'",
        )
        .bind(workspace_id)
        .bind(fan_a)
        .bind(fan_b)
        .bind(merge.id)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "fan identity store failed");
            FanIdentityError::Unavailable
        })?;

        // Re-point pending candidates that named the merged fan onto the
        // survivor so the open evidence still describes live fans. A pair
        // that would self-reference was already resolved above; a pair
        // already recorded (any status) keeps the stale row pending on the
        // tombstone rather than violate the pair UNIQUE.
        for side in ["fan_id_a", "fan_id_b"] {
            let other = if side == "fan_id_a" {
                "fan_id_b"
            } else {
                "fan_id_a"
            };
            sqlx::query(&format!(
                "UPDATE fan_merge_candidates c \
                 SET fan_id_a = LEAST($2, c.{other}), fan_id_b = GREATEST($2, c.{other}) \
                 WHERE c.workspace_id = $1 AND c.{side} = $3 AND c.status = 'pending' \
                 AND c.{other} <> $2 \
                 AND NOT EXISTS (SELECT 1 FROM fan_merge_candidates x \
                     WHERE x.workspace_id = $1 AND x.id <> c.id \
                     AND x.fan_id_a = LEAST($2, c.{other}) \
                     AND x.fan_id_b = GREATEST($2, c.{other}))",
            ))
            .bind(workspace_id)
            .bind(command.survivor_fan_id)
            .bind(command.merged_fan_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                tracing::warn!(%error, "fan identity store failed");
                FanIdentityError::Unavailable
            })?;
        }

        tx.commit()
            .await
            .map_err(|_| FanIdentityError::Unavailable)?;
        Ok(merge.into_view())
    }

    async fn unmerge_fan(
        &self,
        command: &UnmergeFanCommand,
    ) -> Result<FanMergeView, FanIdentityError> {
        let workspace_id = command.workspace_id.into_uuid();
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|_| FanIdentityError::Unavailable)?;

        // Same deferred recipient FK as the merge — replaying moved rows
        // re-points deliveries before their recipient rows exist again.
        sqlx::query("SET CONSTRAINTS communication_campaign_deliveries_recipient_fk DEFERRED")
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                tracing::warn!(%error, "fan identity store failed");
                FanIdentityError::Unavailable
            })?;

        let open = sqlx::query_as::<_, (Uuid, Uuid, Value, String)>(
            "SELECT id, survivor_fan_id, moved, prior_status FROM fan_merges \
             WHERE workspace_id = $1 AND merged_fan_id = $2 AND unmerged_at IS NULL \
             ORDER BY merged_at DESC LIMIT 1 FOR UPDATE",
        )
        .bind(workspace_id)
        .bind(command.merged_fan_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "fan identity store failed");
            FanIdentityError::Unavailable
        })?
        .ok_or(FanIdentityError::NothingToUnmerge)?;
        let (merge_id, survivor_id, moved, prior_status) = open;

        // Replay each table's moved keys back onto the unmerged fan.
        let moved_obj = moved.as_object().cloned().unwrap_or_default();
        for spec in MOVE_SPECS {
            let keys: Vec<String> = moved_obj
                .get(spec.table)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            if keys.is_empty() {
                continue;
            }
            // The audit key is the move's RETURNING expression — a
            // post-update value. For one-row-per-fan tables (ad
            // attribution, location/push preferences) that is the
            // survivor's own fan_id, which still pins the single moved
            // row exactly.
            let key_col = spec
                .move_sql
                .rsplit("RETURNING ")
                .next()
                .unwrap_or("id::text");
            let ws_col = if spec.table == "amplification_deliveries" {
                "to_workspace_id"
            } else {
                "workspace_id"
            };
            let sql = format!(
                "UPDATE {} SET fan_id = $3 WHERE {ws_col} = $1 AND fan_id = $2 \
                 AND {} = ANY($4)",
                spec.table, key_col
            );
            sqlx::query(&sql)
                .bind(workspace_id)
                .bind(survivor_id)
                .bind(command.merged_fan_id)
                .bind(&keys)
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    tracing::warn!(%error, "fan identity store failed");
                    FanIdentityError::Unavailable
                })?;
        }
        if let Some(keys) = moved_obj.get("fan_identifiers").and_then(Value::as_array) {
            let keys: Vec<String> = keys
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            if !keys.is_empty() {
                sqlx::query(
                    "UPDATE fan_identifiers SET fan_id = $3 WHERE workspace_id = $1 \
                     AND fan_id = $2 AND id::text = ANY($4)",
                )
                .bind(workspace_id)
                .bind(survivor_id)
                .bind(command.merged_fan_id)
                .bind(&keys)
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    tracing::warn!(%error, "fan identity store failed");
                    FanIdentityError::Unavailable
                })?;
            }
        }

        // Restore the fan exactly as it was. Mirrored consent rows stay —
        // append-only history recorded that the survivor held them for a while.
        sqlx::query(
            "UPDATE fans SET status = $3, merged_into_fan_id = NULL, merged_at = NULL, \
             updated_at = now() WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace_id)
        .bind(command.merged_fan_id)
        .bind(&prior_status)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "fan identity store failed");
            FanIdentityError::Unavailable
        })?;

        let merge = sqlx::query_as::<_, MergeRow>(
            "UPDATE fan_merges SET unmerged_at = now(), unmerged_by = $4 \
             WHERE workspace_id = $1 AND id = $2 AND unmerged_at IS NULL \
             RETURNING id, survivor_fan_id, merged_fan_id, moved, retained, consents_mirrored, \
                 merged_at, unmerged_at",
        )
        .bind(workspace_id)
        .bind(merge_id)
        .bind(command.merged_fan_id)
        .bind(command.unmerged_by.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "fan identity store failed");
            FanIdentityError::Unavailable
        })?;

        // Reopen the candidates this merge resolved — the undo means the
        // pair is suspect again, and the pair UNIQUE would otherwise keep
        // fresh evidence from ever re-recording it.
        sqlx::query(
            "UPDATE fan_merge_candidates SET status = 'pending', resolved_at = NULL, \
             resolved_merge_id = NULL WHERE workspace_id = $1 AND resolved_merge_id = $2 \
             AND status = 'merged'",
        )
        .bind(workspace_id)
        .bind(merge_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "fan identity store failed");
            FanIdentityError::Unavailable
        })?;

        tx.commit()
            .await
            .map_err(|_| FanIdentityError::Unavailable)?;
        Ok(merge.into_view())
    }

    async fn dismiss_merge_candidate(
        &self,
        command: &DismissMergeCandidateCommand,
    ) -> Result<(), FanIdentityError> {
        let result = sqlx::query(
            "UPDATE fan_merge_candidates SET status = 'dismissed', resolved_at = now() \
             WHERE workspace_id = $1 AND id = $2 AND status = 'pending'",
        )
        .bind(command.workspace_id.into_uuid())
        .bind(command.candidate_id)
        .execute(&self.pool)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "fan identity store failed");
            FanIdentityError::Unavailable
        })?;
        if result.rows_affected() == 0 {
            let exists = sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM fan_merge_candidates WHERE workspace_id = $1 AND id = $2",
            )
            .bind(command.workspace_id.into_uuid())
            .bind(command.candidate_id)
            .fetch_one(&self.pool)
            .await
            .map_err(|error| {
                tracing::warn!(%error, "fan identity store failed");
                FanIdentityError::Unavailable
            })?;
            return Err(if exists == 0 {
                FanIdentityError::NotFound
            } else {
                FanIdentityError::AlreadyResolved
            });
        }
        Ok(())
    }

    async fn list_fan_merges(
        &self,
        workspace_id: WorkspaceId,
        fan_id: Uuid,
    ) -> Result<Vec<FanMergeView>, FanIdentityError> {
        let rows = sqlx::query_as::<_, MergeRow>(
            "SELECT id, survivor_fan_id, merged_fan_id, moved, retained, consents_mirrored, \
                 merged_at, unmerged_at FROM fan_merges \
             WHERE workspace_id = $1 AND (survivor_fan_id = $2 OR merged_fan_id = $2) \
             ORDER BY merged_at DESC LIMIT 50",
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "fan identity store failed");
            FanIdentityError::Unavailable
        })?;
        Ok(rows.into_iter().map(MergeRow::into_view).collect())
    }
}

/// Resolves the canonical `(id, status)` fan pair for a normalized email
/// inside a transaction, locking the resolved fan row `FOR UPDATE`.
///
/// The identifier spine wins: it routes merged-away and secondary addresses
/// to the surviving fan. The fans row is the fallback so a record the spine
/// does not cover still resolves. A merged row is followed through
/// `merged_into_fan_id` until a live fan answers — chain merges are refused
/// at write time, so at most one hop ever exists, but the loop also covers
/// any legacy chain written before that rule.
///
/// Lock order differs from `merge_fans` (data-dependent here, canonical id
/// order there), so a merge racing a resolution can deadlock — Postgres
/// detects and aborts one side, which surfaces as a retryable store error
/// rather than corruption.
///
/// # Errors
///
/// Returns the `sqlx` error; callers map it to their store error.
pub async fn resolve_fan_for_email(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    normalized_email: &str,
) -> Result<Option<(Uuid, String)>, sqlx::Error> {
    let by_identifier = sqlx::query_as::<_, (Uuid, String, Option<Uuid>)>(
        "SELECT f.id, f.status, f.merged_into_fan_id \
         FROM fan_identifiers i \
         JOIN fans f ON f.workspace_id = i.workspace_id AND f.id = i.fan_id \
         WHERE i.workspace_id = $1 AND i.kind = 'email' AND i.value = $2 \
         FOR UPDATE OF f",
    )
    .bind(workspace_id)
    .bind(normalized_email)
    .fetch_optional(&mut **tx)
    .await?;
    let resolved = match by_identifier {
        Some(row) => Some(row),
        None => {
            sqlx::query_as::<_, (Uuid, String, Option<Uuid>)>(
                "SELECT id, status, merged_into_fan_id FROM fans \
                 WHERE workspace_id = $1 AND normalized_email = $2 FOR UPDATE",
            )
            .bind(workspace_id)
            .bind(normalized_email)
            .fetch_optional(&mut **tx)
            .await?
        }
    };

    follow_merge_hops(tx, workspace_id, resolved).await
}

/// Read-path variant of [`resolve_fan_for_email`] — same resolution, no row
/// locks, safe to call from a plain pool outside a transaction.
///
/// # Errors
///
/// Returns the `sqlx` error; callers map it to their store error.
pub async fn resolve_fan_for_email_pool(
    pool: &PgPool,
    workspace_id: Uuid,
    normalized_email: &str,
) -> Result<Option<(Uuid, String)>, sqlx::Error> {
    let resolved = sqlx::query_as::<_, (Uuid, String, Option<Uuid>)>(
        "SELECT f.id, f.status, f.merged_into_fan_id \
         FROM fan_identifiers i \
         JOIN fans f ON f.workspace_id = i.workspace_id AND f.id = i.fan_id \
         WHERE i.workspace_id = $1 AND i.kind = 'email' AND i.value = $2 \
         UNION ALL \
         SELECT f.id, f.status, f.merged_into_fan_id FROM fans f \
         WHERE f.workspace_id = $1 AND f.normalized_email = $2 \
         AND NOT EXISTS ( \
             SELECT 1 FROM fan_identifiers i \
             WHERE i.workspace_id = $1 AND i.kind = 'email' AND i.value = $2) \
         LIMIT 1",
    )
    .bind(workspace_id)
    .bind(normalized_email)
    .fetch_optional(pool)
    .await?;
    match resolved {
        Some(row) => follow_merge_hops_pool(pool, workspace_id, Some(row)).await,
        None => Ok(None),
    }
}

/// Walks `merged_into_fan_id` inside a transaction until a non-merged fan
/// answers. Hard-capped: a cycle can only come from corrupt data, and the
/// merge writer forbids chains.
async fn follow_merge_hops(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    resolved: Option<(Uuid, String, Option<Uuid>)>,
) -> Result<Option<(Uuid, String)>, sqlx::Error> {
    const MAX_HOPS: u8 = 8;
    let mut current = resolved;
    for _ in 0..MAX_HOPS {
        match current {
            Some((_, status, Some(survivor))) if status == "merged" => {
                current = sqlx::query_as::<_, (Uuid, String, Option<Uuid>)>(
                    "SELECT id, status, merged_into_fan_id FROM fans \
                     WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
                )
                .bind(workspace_id)
                .bind(survivor)
                .fetch_optional(&mut **tx)
                .await?;
            }
            Some((id, status, _)) => return Ok(Some((id, status))),
            None => return Ok(None),
        }
    }
    Ok(None)
}

/// Pool variant of the hop walk — no transaction, no locks.
async fn follow_merge_hops_pool(
    pool: &PgPool,
    workspace_id: Uuid,
    resolved: Option<(Uuid, String, Option<Uuid>)>,
) -> Result<Option<(Uuid, String)>, sqlx::Error> {
    const MAX_HOPS: u8 = 8;
    let mut current = resolved;
    for _ in 0..MAX_HOPS {
        match current {
            Some((_, status, Some(survivor))) if status == "merged" => {
                current = sqlx::query_as::<_, (Uuid, String, Option<Uuid>)>(
                    "SELECT id, status, merged_into_fan_id FROM fans \
                     WHERE workspace_id = $1 AND id = $2",
                )
                .bind(workspace_id)
                .bind(survivor)
                .fetch_optional(pool)
                .await?;
            }
            Some((id, status, _)) => return Ok(Some((id, status))),
            None => return Ok(None),
        }
    }
    Ok(None)
}

/// Records a merge candidate for a fan pair, idempotently. Evidence kinds are
/// closed (`CandidateEvidenceKind`); a new signal adds one there first.
/// Returns the candidate id, or `None` when one already exists for the pair.
pub async fn record_merge_candidate(
    pool: &PgPool,
    workspace_id: Uuid,
    fan_a: Uuid,
    fan_b: Uuid,
    evidence: Value,
) -> Result<Option<Uuid>, FanIdentityError> {
    if fan_a == fan_b {
        return Ok(None);
    }
    let (a, b) = canonical_pair(fan_a, fan_b);
    sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO fan_merge_candidates (workspace_id, fan_id_a, fan_id_b, evidence) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (workspace_id, fan_id_a, fan_id_b) DO NOTHING RETURNING id",
    )
    .bind(workspace_id)
    .bind(a)
    .bind(b)
    .bind(evidence)
    .fetch_optional(pool)
    .await
    .map_err(|_| FanIdentityError::Unavailable)
}

/// Transaction variant of [`record_merge_candidate`] for callers already
/// holding the write transaction (e.g. a check-in).
///
/// # Errors
///
/// Returns the `sqlx` error; callers map it to their store error.
pub async fn record_merge_candidate_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    fan_a: Uuid,
    fan_b: Uuid,
    evidence: Value,
) -> Result<Option<Uuid>, sqlx::Error> {
    if fan_a == fan_b {
        return Ok(None);
    }
    let (a, b) = canonical_pair(fan_a, fan_b);
    sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO fan_merge_candidates (workspace_id, fan_id_a, fan_id_b, evidence) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (workspace_id, fan_id_a, fan_id_b) DO NOTHING RETURNING id",
    )
    .bind(workspace_id)
    .bind(a)
    .bind(b)
    .bind(evidence)
    .fetch_optional(&mut **tx)
    .await
}
