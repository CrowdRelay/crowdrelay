//! Audience Graph persistence.
//!
//! All SQL for the discovery pipeline lives here so the HTTP layer stays free
//! of write statements (see the api-sql ratchet) and the worker sweep can
//! share the exact same statements. Stage transitions are validated in
//! `crowdrelay_domain::audience_graph` first; the UPDATE carries a `FROM`
//! guard on the current stage anyway, so a concurrent operator move loses the
//! race cleanly instead of double-applying.

use std::collections::HashMap;

use sqlx::{FromRow, PgPool, Postgres, Transaction};
use uuid::Uuid;

use crowdrelay_domain::audience_graph::{self, OutreachStage, PlaceKind};

#[derive(Clone)]
pub struct PostgresAudienceGraphRepository {
    pool: PgPool,
}

#[derive(Debug, thiserror::Error)]
pub enum AudienceGraphError {
    #[error("place or pipeline not found")]
    NotFound,
    /// The requested stage move is not part of the pipeline.
    #[error("pipeline move {current:?} -> {target:?} does not exist")]
    InvalidTransition {
        current: OutreachStage,
        target: OutreachStage,
    },
    /// The place's cooldown has not lapsed yet.
    #[error("place cooldown runs until {next_eligible_at}")]
    CooldownActive {
        next_eligible_at: time::OffsetDateTime,
    },
    #[error("audience graph database operation failed")]
    Database(sqlx::Error),
}

impl AudienceGraphError {
    fn unexpected(error: sqlx::Error) -> Self {
        tracing::error!(error = %error, "audience graph persistence failed");
        AudienceGraphError::Database(error)
    }
}

#[derive(Debug, FromRow)]
pub struct PlaceRow {
    pub id: Uuid,
    pub place_kind: String,
    pub platform: String,
    pub name: String,
    pub url: String,
    pub country_code: Option<String>,
    pub language: Option<String>,
    pub genres: Vec<String>,
    pub member_count: Option<i32>,
    pub activity_bp: Option<i32>,
    pub status: String,
    pub notes: Option<String>,
    // Joined from discovery_place_rules (nullable when no rules are attached).
    pub self_promo_ratio_percent: Option<i16>,
    pub contact_channel: Option<String>,
    pub contact_target: Option<String>,
    pub requires_approval: Option<bool>,
    pub cooldown_days: Option<i16>,
    pub rules_summary: Option<String>,
    pub rules_verified_at: Option<time::OffsetDateTime>,
    // Joined from discovery_outreach.
    pub stage: Option<String>,
    pub next_eligible_at: Option<time::OffsetDateTime>,
    pub last_action_at: Option<time::OffsetDateTime>,
}

const PLACE_SELECT_BASE: &str = r#"
    SELECT place.id, place.place_kind, place.platform, place.name, place.url,
           place.country_code, place.language, place.genres, place.member_count,
           place.activity_bp, place.status, place.notes,
           rules.self_promo_ratio_percent, rules.contact_channel,
           rules.contact_target, rules.requires_approval, rules.cooldown_days,
           rules.rules_summary, rules.verified_at AS rules_verified_at,
           outreach.stage, outreach.next_eligible_at, outreach.last_action_at
    FROM discovery_places AS place
    LEFT JOIN discovery_place_rules AS rules ON rules.place_id = place.id
    LEFT JOIN discovery_outreach AS outreach ON outreach.place_id = place.id
"#;

/// A single scan/observation to attach to a place. Raw payloads land verbatim.
#[derive(Debug)]
pub struct EvidenceInput<'a> {
    pub evidence_kind: &'a str,
    pub method: &'a str,
    pub confidence_bp: i32,
    pub payload: &'a serde_json::Value,
}

#[derive(Debug)]
pub struct UpsertPlaceInput<'a> {
    pub workspace_id: Uuid,
    pub place_kind: PlaceKind,
    pub platform: &'a str,
    pub name: &'a str,
    pub url: &'a str,
    pub country_code: Option<&'a str>,
    pub language: Option<&'a str>,
    pub genres: &'a [String],
    pub member_count: Option<i32>,
    pub activity_bp: Option<i32>,
    pub notes: Option<&'a str>,
}

#[derive(Debug, Clone, Copy)]
pub struct PlaceRulesInput<'a> {
    pub self_promo_ratio_percent: Option<i16>,
    pub contact_channel: Option<&'a str>,
    pub contact_target: Option<&'a str>,
    pub requires_approval: bool,
    pub cooldown_days: i16,
    pub rules_summary: Option<&'a str>,
}

#[must_use]
pub fn place_kind_or_other(value: &str) -> PlaceKind {
    PlaceKind::from_storage(value).unwrap_or(PlaceKind::Other)
}

impl PostgresAudienceGraphRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Inserts a place or refreshes the mutable facts of an existing one keyed
    /// by `(workspace_id, platform, url)`. Identity fields (kind) and the URL
    /// itself never change through an upsert; correct a wrong kind by archiving
    /// the row and registering the corrected one.
    pub async fn upsert_place(
        &self,
        input: &UpsertPlaceInput<'_>,
    ) -> Result<Uuid, AudienceGraphError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(AudienceGraphError::unexpected)?;
        let id = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO discovery_places (
                workspace_id, place_kind, platform, name, url,
                country_code, language, genres, member_count, activity_bp,
                status, notes
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'active', $11)
            ON CONFLICT (workspace_id, platform, url) DO UPDATE SET
                name = EXCLUDED.name,
                genres = EXCLUDED.genres,
                member_count = COALESCE(EXCLUDED.member_count, discovery_places.member_count),
                activity_bp = COALESCE(EXCLUDED.activity_bp, discovery_places.activity_bp),
                country_code = COALESCE(EXCLUDED.country_code, discovery_places.country_code),
                language = COALESCE(EXCLUDED.language, discovery_places.language),
                notes = COALESCE(EXCLUDED.notes, discovery_places.notes),
                status = 'active',
                updated_at = now()
            RETURNING id
            "#,
        )
        .bind(input.workspace_id)
        .bind(input.place_kind.as_str())
        .bind(input.platform)
        .bind(input.name)
        .bind(input.url)
        .bind(input.country_code)
        .bind(input.language)
        .bind(input.genres)
        .bind(input.member_count)
        .bind(input.activity_bp)
        .bind(input.notes)
        .fetch_one(&mut *tx)
        .await
        .map_err(AudienceGraphError::unexpected)?;
        // Every known place enters the pipeline at `discovered`; the unique
        // constraint on place_id makes this idempotent.
        sqlx::query(
            r#"
            INSERT INTO discovery_outreach (workspace_id, place_id, stage)
            VALUES ($1, $2, 'discovered')
            ON CONFLICT (place_id) DO NOTHING
            "#,
        )
        .bind(input.workspace_id)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(AudienceGraphError::unexpected)?;
        tx.commit().await.map_err(AudienceGraphError::unexpected)?;
        Ok(id)
    }

    /// Bulk ingest used by scan tooling: deduped places plus their raw
    /// evidence rows commit atomically. Returns place ids keyed by the input
    /// order they were provided in.
    pub async fn import_scan_batch(
        &self,
        places: &[UpsertPlaceInput<'_>],
        evidence_by_index: &HashMap<usize, Vec<EvidenceInput<'_>>>,
    ) -> Result<Vec<Uuid>, AudienceGraphError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(AudienceGraphError::unexpected)?;
        let mut ids = Vec::with_capacity(places.len());
        for (index, input) in places.iter().enumerate() {
            let id = Self::upsert_place_tx(&mut tx, input).await?;
            if let Some(evidence) = evidence_by_index.get(&index) {
                for item in evidence {
                    sqlx::query(
                        r#"
                        INSERT INTO discovery_place_evidence (
                            workspace_id, place_id, evidence_kind, method, confidence_bp, payload
                        )
                        VALUES ($1, $2, $3, $4, $5, $6)
                        "#,
                    )
                    .bind(input.workspace_id)
                    .bind(id)
                    .bind(item.evidence_kind)
                    .bind(item.method)
                    .bind(item.confidence_bp)
                    .bind(item.payload)
                    .execute(&mut *tx)
                    .await
                    .map_err(AudienceGraphError::unexpected)?;
                }
            }
            ids.push(id);
        }
        tx.commit().await.map_err(AudienceGraphError::unexpected)?;
        Ok(ids)
    }

    pub async fn attach_rules(
        &self,
        workspace_id: Uuid,
        place_id: Uuid,
        rules: &PlaceRulesInput<'_>,
        verify: bool,
    ) -> Result<(), AudienceGraphError> {
        // The SELECT carries the workspace guard, so rules can only ever be
        // attached to a place inside the caller's own workspace.
        let result = sqlx::query(
            r#"
            INSERT INTO discovery_place_rules (
                place_id, self_promo_ratio_percent, contact_channel,
                contact_target, requires_approval, cooldown_days,
                rules_summary, verified_at
            )
            SELECT place.id, $3, $4, $5, $6, $7, $8,
                   CASE WHEN $9 THEN now() ELSE NULL END
            FROM discovery_places AS place
            WHERE place.id = $1 AND place.workspace_id = $2
            ON CONFLICT (place_id) DO UPDATE SET
                self_promo_ratio_percent = EXCLUDED.self_promo_ratio_percent,
                contact_channel = EXCLUDED.contact_channel,
                contact_target = EXCLUDED.contact_target,
                requires_approval = EXCLUDED.requires_approval,
                cooldown_days = EXCLUDED.cooldown_days,
                rules_summary = EXCLUDED.rules_summary,
                verified_at = CASE WHEN $9 THEN now() ELSE NULL END,
                updated_at = now()
            "#,
        )
        .bind(place_id)
        .bind(workspace_id)
        .bind(rules.self_promo_ratio_percent)
        .bind(rules.contact_channel)
        .bind(rules.contact_target)
        .bind(rules.requires_approval)
        .bind(rules.cooldown_days)
        .bind(rules.rules_summary)
        .bind(verify)
        .execute(&self.pool)
        .await
        .map_err(AudienceGraphError::unexpected)?;
        if result.rows_affected() == 0 {
            return Err(AudienceGraphError::NotFound);
        }
        Ok(())
    }

    pub async fn append_evidence(
        &self,
        workspace_id: Uuid,
        place_id: Uuid,
        evidence: &EvidenceInput<'_>,
    ) -> Result<(), AudienceGraphError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(AudienceGraphError::unexpected)?;
        Self::assert_place_exists(&mut tx, workspace_id, place_id).await?;
        sqlx::query(
            r#"
            INSERT INTO discovery_place_evidence (
                workspace_id, place_id, evidence_kind, method, confidence_bp, payload
            )
            VALUES ($1, $2, $3, $4, $5, $6)
            "#,
        )
        .bind(workspace_id)
        .bind(place_id)
        .bind(evidence.evidence_kind)
        .bind(evidence.method)
        .bind(evidence.confidence_bp)
        .bind(evidence.payload)
        .execute(&mut *tx)
        .await
        .map_err(AudienceGraphError::unexpected)?;
        tx.commit().await.map_err(AudienceGraphError::unexpected)?;
        Ok(())
    }

    pub async fn list_places(
        &self,
        workspace_id: Uuid,
        kind: Option<PlaceKind>,
        status: Option<&str>,
        stage: Option<OutreachStage>,
        limit: i64,
    ) -> Result<Vec<PlaceRow>, AudienceGraphError> {
        let rows = sqlx::query_as::<_, PlaceRow>(&format!(
            "{PLACE_SELECT_BASE} \
             WHERE place.workspace_id = $1 \
               AND ($2::text IS NULL OR place.place_kind = $2) \
               AND ($3::text IS NULL OR place.status = $3) \
               AND ($4::text IS NULL OR outreach.stage = $4) \
             ORDER BY place.updated_at DESC, place.id \
             LIMIT $5"
        ))
        .bind(workspace_id)
        .bind(kind.map(|value| value.as_str()))
        .bind(status)
        .bind(stage.map(|value| value.as_str()))
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(AudienceGraphError::unexpected)?;
        Ok(rows)
    }

    pub async fn place_detail(
        &self,
        workspace_id: Uuid,
        place_id: Uuid,
    ) -> Result<PlaceRow, AudienceGraphError> {
        sqlx::query_as::<_, PlaceRow>(&format!(
            "{PLACE_SELECT_BASE} WHERE place.workspace_id = $1 AND place.id = $2"
        ))
        .bind(workspace_id)
        .bind(place_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(AudienceGraphError::unexpected)?
        .ok_or(AudienceGraphError::NotFound)
    }

    /// Moves the pipeline forward inside an existing transaction (worker sweeps
    /// compose several moves atomically). Domain policy decides whether the
    /// transition exists; SQL decides whether it still does by the time the row
    /// lock lands, so a concurrent operator move loses the race cleanly.
    pub async fn advance_outreach_in_tx(
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: Uuid,
        place_id: Uuid,
        current_stage: OutreachStage,
        target_stage: OutreachStage,
        outcome_notes: Option<&str>,
    ) -> Result<(), AudienceGraphError> {
        if !audience_graph::can_advance(current_stage, target_stage) {
            return Err(AudienceGraphError::InvalidTransition {
                current: current_stage,
                target: target_stage,
            });
        }
        let result = sqlx::query(
            r#"
            UPDATE discovery_outreach AS outreach
            SET stage = $3,
                last_action_at = now(),
                next_eligible_at = now()
                    + make_interval(days => COALESCE(rules.cooldown_days, 14)::int),
                outcome_notes = COALESCE($5, outreach.outcome_notes),
                updated_at = now()
            FROM discovery_places AS place
            LEFT JOIN discovery_place_rules AS rules ON rules.place_id = place.id
            WHERE outreach.place_id = place.id
              AND place.id = $2
              AND place.workspace_id = $1
              AND outreach.stage = $4
            "#,
        )
        .bind(workspace_id)
        .bind(place_id)
        .bind(target_stage.as_str())
        .bind(current_stage.as_str())
        .bind(outcome_notes)
        .execute(&mut **transaction)
        .await
        .map_err(AudienceGraphError::unexpected)?;
        if result.rows_affected() == 0 {
            // Either the stage moved under us or the place is gone; report the
            // stale-stage case as an invalid transition with fresh state.
            return Err(AudienceGraphError::InvalidTransition {
                current: current_stage,
                target: target_stage,
            });
        }
        Ok(())
    }

    /// Single-shot pipeline move owned by one HTTP request.
    pub async fn advance_outreach(
        &self,
        workspace_id: Uuid,
        place_id: Uuid,
        current_stage: OutreachStage,
        target_stage: OutreachStage,
        outcome_notes: Option<&str>,
    ) -> Result<(), AudienceGraphError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(AudienceGraphError::unexpected)?;
        Self::advance_outreach_in_tx(
            &mut tx,
            workspace_id,
            place_id,
            current_stage,
            target_stage,
            outcome_notes,
        )
        .await?;
        tx.commit().await.map_err(AudienceGraphError::unexpected)?;
        Ok(())
    }

    /// Marks long-silent relationships dormant. Deterministic decay used by
    /// the worker sweep; returns how many pipelines were retired this pass.
    pub async fn decay_dormant(
        &self,
        workspace_id: Uuid,
        inactive_for: time::Duration,
        limit: i64,
    ) -> Result<u64, AudienceGraphError> {
        let result = sqlx::query(
            r#"
            UPDATE discovery_outreach AS outreach
            SET stage = 'dormant', updated_at = now()
            WHERE outreach.id IN (
                SELECT candidate.id
                FROM discovery_outreach AS candidate
                WHERE candidate.workspace_id = $1
                  AND candidate.stage IN ('researched', 'contacted', 'replied', 'negotiating')
                  AND COALESCE(candidate.last_action_at, candidate.created_at)
                      < now() - make_interval(days => $2::int)
                ORDER BY COALESCE(candidate.last_action_at, candidate.created_at), candidate.id
                LIMIT $3
                FOR UPDATE
            )
            "#,
        )
        .bind(workspace_id)
        .bind(i32::try_from(inactive_for.whole_days()).unwrap_or(i32::MAX))
        .bind(limit)
        .execute(&self.pool)
        .await
        .map_err(AudienceGraphError::unexpected)?;
        Ok(result.rows_affected())
    }

    async fn assert_place_exists(
        tx: &mut Transaction<'_, Postgres>,
        workspace_id: Uuid,
        place_id: Uuid,
    ) -> Result<(), AudienceGraphError> {
        let found = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM discovery_places WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace_id)
        .bind(place_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(AudienceGraphError::unexpected)?;
        if found.is_none() {
            return Err(AudienceGraphError::NotFound);
        }
        Ok(())
    }

    async fn upsert_place_tx(
        tx: &mut Transaction<'_, Postgres>,
        input: &UpsertPlaceInput<'_>,
    ) -> Result<Uuid, AudienceGraphError> {
        let id = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO discovery_places (
                workspace_id, place_kind, platform, name, url,
                country_code, language, genres, member_count, activity_bp,
                status, notes
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'active', $11)
            ON CONFLICT (workspace_id, platform, url) DO UPDATE SET
                name = EXCLUDED.name,
                genres = EXCLUDED.genres,
                member_count = COALESCE(EXCLUDED.member_count, discovery_places.member_count),
                activity_bp = COALESCE(EXCLUDED.activity_bp, discovery_places.activity_bp),
                country_code = COALESCE(EXCLUDED.country_code, discovery_places.country_code),
                language = COALESCE(EXCLUDED.language, discovery_places.language),
                notes = COALESCE(EXCLUDED.notes, discovery_places.notes),
                status = 'active',
                updated_at = now()
            RETURNING id
            "#,
        )
        .bind(input.workspace_id)
        .bind(input.place_kind.as_str())
        .bind(input.platform)
        .bind(input.name)
        .bind(input.url)
        .bind(input.country_code)
        .bind(input.language)
        .bind(input.genres)
        .bind(input.member_count)
        .bind(input.activity_bp)
        .bind(input.notes)
        .fetch_one(&mut **tx)
        .await
        .map_err(AudienceGraphError::unexpected)?;
        sqlx::query(
            r#"
            INSERT INTO discovery_outreach (workspace_id, place_id, stage)
            VALUES ($1, $2, 'discovered')
            ON CONFLICT (place_id) DO NOTHING
            "#,
        )
        .bind(input.workspace_id)
        .bind(id)
        .execute(&mut **tx)
        .await
        .map_err(AudienceGraphError::unexpected)?;
        Ok(id)
    }
}
