//! The suggestion engine's repository side — gather the inputs, run the
//! deterministic ranker, persist the survivors.
//!
//! `crowdrelay-brain::content_suggestions` is pure; this module is where
//! its inputs come from. The reads happen on a consistent snapshot inside
//! one transaction under the same per-workspace advisory lock the trend
//! refresh uses — two sweeps racing a suggestion pass cannot interleave
//! reads and writes into a duplicate raise.

use std::collections::{BTreeMap, BTreeSet};

use crowdrelay_brain::content_suggestions::{
    DEFAULT_LIMIT, RankingInputs, ReachSnapshot, ScheduledProduction, rank_suggestions,
};
use crowdrelay_domain::{
    ContentSuggestionId, WorkspaceId,
    content_engine::{ContentSuggestion, SuggestionStatus},
};
use sqlx::FromRow;
use time::Date;
use uuid::Uuid;

use crate::content_engine::{PostgresContentEngineRepository, Result, SuggestionRow};

#[derive(Debug, FromRow)]
struct ReachRow {
    communities: Vec<String>,
    press_contacts: i64,
    peers: Vec<String>,
    consented_fans: i64,
}

#[derive(Debug, FromRow)]
struct OpenSuggestionKeyRow {
    format_key: Option<String>,
}

#[derive(Debug, FromRow)]
struct HistoryRow {
    format_key: String,
    suggestions: i64,
    outcomes: i64,
}

impl PostgresContentEngineRepository {
    /// What the promise can honestly name: admitted community names,
    /// admitted-or-promoted press-route candidates, confirmed peers (a
    /// collaboration's "other audience" is nameable), and fans reachable
    /// under a marketing consent — the same `reachable_consented` the KPI
    /// view computes. `fan_consents` is append-only, so the count reads
    /// each fan's *latest* marketing row: a grant followed by a withdrawal
    /// is not consent.
    async fn reach_snapshot(&self, workspace_id: WorkspaceId) -> Result<ReachSnapshot> {
        let row = sqlx::query_as::<_, ReachRow>(
            r#"
            SELECT
                COALESCE(
                    (SELECT array_agg(name ORDER BY name)
                     FROM discovery_places
                     WHERE workspace_id = $1 AND status = 'active'),
                    ARRAY[]::text[]
                ) AS communities,
                (SELECT count(*) FROM viryaos_outreach_candidates
                 WHERE workspace_id = $1
                   AND status IN ('admitted','promoted')
                   AND target_kind IN ('press','radio','media_patronage')
                ) AS press_contacts,
                COALESCE(
                    (SELECT array_agg(name ORDER BY name)
                     FROM viryaos_peers
                     WHERE workspace_id = $1 AND status = 'confirmed'),
                    ARRAY[]::text[]
                ) AS peers,
                (SELECT count(*) FROM fans f
                 WHERE f.workspace_id = $1 AND f.status = 'active'
                   AND EXISTS (
                       SELECT 1 FROM fan_consents c
                       WHERE c.workspace_id = f.workspace_id
                         AND c.fan_id = f.id
                         AND c.purpose = 'marketing'
                         AND c.granted
                         AND c.id = (
                             SELECT newest.id FROM fan_consents newest
                             WHERE newest.workspace_id = f.workspace_id
                               AND newest.fan_id = f.id
                               AND newest.purpose = 'marketing'
                             ORDER BY newest.recorded_at DESC, newest.id DESC
                             LIMIT 1
                         )
                   )
                ) AS consented_fans
            "#,
        )
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(ReachSnapshot {
            communities: row.communities,
            press_contacts: u32::try_from(row.press_contacts).unwrap_or(u32::MAX),
            consented_fans: u32::try_from(row.consented_fans).unwrap_or(u32::MAX),
            peers: row.peers,
        })
    }

    /// Format keys written into the spine of an approved-or-active arc,
    /// mapped to the arc they serve — the suggestion names its arc because
    /// "the operator can always tell" is the column's reason for existing.
    /// When several arcs name the same beat the newest wins.
    async fn arc_format_keys(&self, workspace_id: WorkspaceId) -> Result<BTreeMap<String, Uuid>> {
        #[derive(FromRow)]
        struct ArcBeatRow {
            format_key: String,
            arc_id: Uuid,
        }
        let rows = sqlx::query_as::<_, ArcBeatRow>(
            r#"
            SELECT DISTINCT ON (beat->>'format_key')
                   beat->>'format_key' AS format_key,
                   id AS arc_id
            FROM viryaos_arcs,
                 jsonb_array_elements(spine) AS beat
            WHERE workspace_id = $1
              AND status IN ('approved','active')
              AND jsonb_typeof(spine) = 'array'
              AND beat ? 'format_key'
              AND beat->>'format_key' IS NOT NULL
            ORDER BY beat->>'format_key', created_at DESC
            "#,
        )
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| (row.format_key, row.arc_id))
            .collect())
    }

    /// How often each format has been suggested and resolved — novelty
    /// and information-gain inputs. Outcome counts join through the
    /// suggestion because the outcome table carries no format key.
    async fn format_history(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<(BTreeMap<String, u32>, BTreeMap<String, u32>)> {
        let rows = sqlx::query_as::<_, HistoryRow>(
            r#"
            SELECT s.format_key,
                   count(DISTINCT s.id) AS suggestions,
                   count(o.id) AS outcomes
            FROM viryaos_content_suggestions s
            LEFT JOIN viryaos_suggestion_outcomes o
              ON o.workspace_id = s.workspace_id
             AND o.suggestion_id = s.id
            WHERE s.workspace_id = $1 AND s.format_key IS NOT NULL
            GROUP BY s.format_key
            "#,
        )
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await?;
        let mut suggestions = BTreeMap::new();
        let mut outcomes = BTreeMap::new();
        for row in rows {
            suggestions.insert(
                row.format_key.clone(),
                u32::try_from(row.suggestions).unwrap_or(u32::MAX),
            );
            outcomes.insert(
                row.format_key,
                u32::try_from(row.outcomes).unwrap_or(u32::MAX),
            );
        }
        Ok((suggestions, outcomes))
    }

    /// One ranking pass: gather inputs, rank, persist the survivors as
    /// `raised` suggestions. Returns the rows written — an empty vec is a
    /// truthful "nothing worth the band's time today".
    ///
    /// The input reads happen before the transaction — they are a loose
    /// snapshot of slowly-changing state (formats, trends, reach), and
    /// holding a connection across them would starve a one-connection
    /// pool. The correctness-critical read — which formats are already
    /// open — runs *inside* the transaction under the advisory lock, so
    /// two concurrent passes cannot interleave into a double-raise. The
    /// lock key differs from the trend refresh's on purpose: a suggestion
    /// pass only reads `viryaos_content_trends` after that writer commits,
    /// so serializing the two would buy nothing.
    pub async fn refresh_suggestions(
        &self,
        workspace_id: WorkspaceId,
        today: Date,
    ) -> Result<Vec<ContentSuggestion>> {
        let ws = workspace_id.into_uuid();

        let profile = self.capability_profile(workspace_id).await?;
        let formats = self.list_format_entries().await?;
        let trends = self.list_trends(workspace_id).await?;
        let events = self.upcoming_production_events(workspace_id, today).await?;
        let reach = self.reach_snapshot(workspace_id).await?;
        let arc_keys = self.arc_format_keys(workspace_id).await?;
        let (suggestion_counts, outcome_counts) = self.format_history(workspace_id).await?;

        let production: Vec<ScheduledProduction> = events
            .iter()
            .map(|event| ScheduledProduction {
                id: event.id.into_uuid().to_string(),
                kind: event.kind,
                scheduled_for: event.scheduled_for,
            })
            .collect();

        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::text))")
            .bind(format!("{ws}:suggestions"))
            .execute(&mut *tx)
            .await?;

        // A raised suggestion whose window passed is a dead ask, and the open
        // queue counts it as live until something resolves it — three lapsed
        // rows would silently hold every slot forever. Expiry is a first-class
        // outcome, resolved here inside the lock before headroom is counted,
        // so a pass always sees the queue as it actually is. `approved` rows
        // are the band's commitment, not an ask — they do not lapse.
        let lapsed = sqlx::query_scalar::<_, Uuid>(
            r#"
            UPDATE viryaos_content_suggestions
            SET status = 'expired', updated_at = now()
            WHERE workspace_id = $1 AND status = 'raised'
              AND expires_at IS NOT NULL AND expires_at <= now()
            RETURNING id
            "#,
        )
        .bind(ws)
        .fetch_all(&mut *tx)
        .await?;
        for suggestion_id in lapsed {
            sqlx::query(
                r#"
                INSERT INTO viryaos_suggestion_outcomes (
                    workspace_id, suggestion_id, outcome, decided_by, reason
                ) VALUES ($1, $2, 'expired', 'system', 'the window this beat was for has passed')
                "#,
            )
            .bind(ws)
            .bind(suggestion_id)
            .execute(&mut *tx)
            .await?;
        }

        // The open queue is the Pareto cut: it holds at most `limit` live
        // suggestions, so a pass tops up to the limit rather than appending
        // it. `open_format_keys` suppresses re-raising; `open_count` (which
        // counts bespoke keyless rows too) is the headroom.
        let open_rows = sqlx::query_as::<_, OpenSuggestionKeyRow>(
            r#"
            SELECT format_key FROM viryaos_content_suggestions
            WHERE workspace_id = $1 AND status IN ('raised','approved')
            "#,
        )
        .bind(ws)
        .fetch_all(&mut *tx)
        .await?;
        let open_count = open_rows.len();
        let open_format_keys: BTreeSet<String> = open_rows
            .into_iter()
            .filter_map(|row| row.format_key)
            .collect();

        let headroom = DEFAULT_LIMIT.saturating_sub(open_count);
        if headroom == 0 {
            tx.commit().await?;
            return Ok(Vec::new());
        }

        let ranked = rank_suggestions(&RankingInputs {
            formats: &formats,
            profile: &profile,
            trends: &trends,
            production: &production,
            open_format_keys: &open_format_keys,
            arc_format_keys: &arc_keys,
            outcome_counts: &outcome_counts,
            suggestion_counts: &suggestion_counts,
            reach: &reach,
            weights: Default::default(),
            today,
        });

        // Rank, then cut — and name the tail out loud so "these two carry
        // most of it" is a claim the operator can check.
        let (top, tail) = ranked.split_at(headroom.min(ranked.len()));
        if !tail.is_empty() {
            tracing::info!(
                tail = ?tail.iter().map(|s| s.format_key.as_str()).collect::<Vec<_>>(),
                "other feasible formats this pass; the raised ones carry most of it"
            );
        }

        let mut raised = Vec::with_capacity(top.len());
        for scored in top {
            // A covered suggestion's window is the production day itself —
            // after it passes, the near-free price is a lie and the row
            // expires.
            let expires_at = scored
                .suggested_before
                .map(|day| day.midnight().assume_utc() + time::Duration::days(1));
            let row = sqlx::query_as::<_, SuggestionRow>(
                r#"
                INSERT INTO viryaos_content_suggestions (
                    id, workspace_id, arc_id, format_key, concept, reason,
                    evidence, suggested_after, suggested_before, effort,
                    proposed_assignee_member_id, distribution_promise,
                    expires_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, NULL, $8, $9, NULL, $10, $11)
                RETURNING *
                "#,
            )
            .bind(ContentSuggestionId::new().into_uuid())
            .bind(ws)
            .bind(scored.arc_id)
            .bind(&scored.format_key)
            .bind(&scored.concept)
            .bind(&scored.reason)
            .bind(&scored.evidence)
            .bind(scored.suggested_before)
            .bind(scored.effort.as_str())
            .bind(&scored.distribution_promise)
            .bind(expires_at)
            .fetch_one(&mut *tx)
            .await?;
            raised.push(ContentSuggestion::try_from(row)?);
        }
        tx.commit().await?;
        debug_assert!(
            raised.iter().all(|s| s.status == SuggestionStatus::Raised),
            "fresh suggestions start raised"
        );
        Ok(raised)
    }
}
