//! The suggestion engine's repository side — gather the inputs, run the
//! deterministic ranker, persist the survivors.
//!
//! `crowdrelay-brain::content_suggestions` is pure; this module is where
//! its inputs come from. Slowly-changing inputs (catalogue, trends,
//! reach) are read before the transaction as a deliberately loose
//! snapshot; the correctness-critical reads — open suggestions, declined
//! formats, headroom — run inside it under the per-workspace advisory
//! lock `"{ws}:suggestions"`, so two sweeps racing a suggestion pass
//! cannot interleave into a duplicate raise.

use std::collections::{BTreeMap, BTreeSet};

use crowdrelay_brain::content_suggestions::{
    DEFAULT_LIMIT, FormatYield, RankingInputs, ReachSnapshot, ScheduledProduction, YIELD_EMA_ALPHA,
    is_stale, rank_suggestions,
};
use crowdrelay_domain::{
    ContentSuggestionId, WorkspaceId,
    content_engine::{ContentSuggestion, SuggestionStatus},
};
use sqlx::FromRow;
use time::Date;
use uuid::Uuid;

use crate::content_engine::{PostgresContentEngineRepository, Result, SuggestionRow};
use serde_json::json;

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

/// How long a band's "not for us" keeps a format out of the queue. An arc
/// decline is "not this season" and cools the anchor for a fortnight; a
/// suggestion decline is the format itself, so the window runs half a
/// season — long enough that re-asking reads as not listening, short
/// enough that a genuinely changed band can be argued back by evidence.
pub(crate) const TASTE_COOLDOWN_DAYS: i64 = 42;

#[derive(Debug, FromRow)]
struct HistoryRow {
    format_key: String,
    suggestions: i64,
    outcomes: i64,
    /// Outcomes where the band actually made the thing — `done` or
    /// `done_differently`. `declined`/`expired` are attempts, not
    /// productions, and the stale rule only credits production.
    produced: i64,
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
                (SELECT count(*) FROM outreach_candidates
                 WHERE workspace_id = $1
                   AND status IN ('admitted','promoted')
                   AND target_kind IN ('press','radio','media_patronage')
                ) AS press_contacts,
                COALESCE(
                    (SELECT array_agg(name ORDER BY name)
                     FROM peers
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
            FROM arcs,
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
    pub(crate) async fn format_history<'e, E>(
        &self,
        executor: E,
        workspace_id: WorkspaceId,
    ) -> Result<(
        BTreeMap<String, u32>,
        BTreeMap<String, u32>,
        BTreeMap<String, u32>,
    )>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        let rows = sqlx::query_as::<_, HistoryRow>(
            r#"
            SELECT s.format_key,
                   count(DISTINCT s.id) AS suggestions,
                   count(o.id) AS outcomes,
                   count(o.id) FILTER (WHERE o.outcome IN ('done', 'done_differently'))
                       AS produced
            FROM content_suggestions s
            LEFT JOIN suggestion_outcomes o
              ON o.workspace_id = s.workspace_id
             AND o.suggestion_id = s.id
            WHERE s.workspace_id = $1 AND s.format_key IS NOT NULL
            GROUP BY s.format_key
            "#,
        )
        .bind(workspace_id.into_uuid())
        .fetch_all(executor)
        .await?;
        let mut suggestions = BTreeMap::new();
        let mut outcomes = BTreeMap::new();
        let mut produced = BTreeMap::new();
        for row in rows {
            suggestions.insert(
                row.format_key.clone(),
                u32::try_from(row.suggestions).unwrap_or(u32::MAX),
            );
            outcomes.insert(
                row.format_key.clone(),
                u32::try_from(row.outcomes).unwrap_or(u32::MAX),
            );
            produced.insert(
                row.format_key,
                u32::try_from(row.produced).unwrap_or(u32::MAX),
            );
        }
        Ok((suggestions, outcomes, produced))
    }

    /// What the band's own resolved outcomes measured per format — an EMA
    /// of reported `results.new_fans` ordered by `resolved_at`, so recent
    /// reports outweigh early ones, alongside how many outcomes reported a
    /// figure at all. Only outcomes where the band actually made something
    /// count — a `declined` or `expired` row cannot carry a real
    /// measurement, and crediting one would teach the yield a production
    /// that never happened.
    pub(crate) async fn format_yields<'e, E>(
        &self,
        executor: E,
        workspace_id: WorkspaceId,
    ) -> Result<BTreeMap<String, FormatYield>>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        // `jsonb_typeof` guards the cast — a string "12" or a stray object
        // in `results` cannot abort the pass.
        let rows = sqlx::query_as::<_, (String, f64)>(
            r#"
            SELECT s.format_key,
                   (o.results->>'new_fans')::double precision AS new_fans
            FROM content_suggestions s
            JOIN suggestion_outcomes o
              ON o.workspace_id = s.workspace_id
             AND o.suggestion_id = s.id
            WHERE s.workspace_id = $1 AND s.format_key IS NOT NULL
              AND o.outcome IN ('done', 'done_differently')
              AND jsonb_typeof(o.results->'new_fans') = 'number'
            ORDER BY s.format_key, o.resolved_at, o.id
            "#,
        )
        .bind(workspace_id.into_uuid())
        .fetch_all(executor)
        .await?;
        let mut format_yield: BTreeMap<String, FormatYield> = BTreeMap::new();
        for (format_key, new_fans) in rows {
            let entry = format_yield.entry(format_key).or_insert(FormatYield {
                measured_fans_ema: new_fans,
                measured: 0,
            });
            entry.measured_fans_ema = if entry.measured == 0 {
                new_fans
            } else {
                YIELD_EMA_ALPHA * new_fans + (1.0 - YIELD_EMA_ALPHA) * entry.measured_fans_ema
            };
            entry.measured += 1;
        }
        Ok(format_yield)
    }

    /// 5.6 — shared learning, respecting per-band taste. Productions of each
    /// format pooled across the act's same-style siblings in the same
    /// organisation: the label's own ledger arguing for a format its other
    /// bands of the same shape already made work.
    ///
    /// Two absences are honest zeros:
    ///
    /// * **No organisation or no declared `act_style`** — nothing to match
    ///   on, so nothing pools. A band that never said what it sounds like
    ///   cannot claim taste-kinship, and guessing it would be exactly the
    ///   inference 5.21 refused.
    /// * **No same-style sibling produced it** — the map simply lacks the
    ///   key; the ranker's floor (`SIBLING_PROOF_MIN`) turns one anecdote
    ///   into no lift.
    ///
    /// The taste gate is normalised equality on the declared descriptor —
    /// lowercase, whitespace-folded. Only `done`/`done_differently` count:
    /// the same production rule the stale check credits, so a sibling's
    /// declined or lapsed attempt teaches nothing.
    async fn sibling_produced_counts(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<BTreeMap<String, u32>> {
        let ws = workspace_id.into_uuid();
        let (organization_id,): (Option<Uuid>,) =
            sqlx::query_as("SELECT organization_id FROM workspaces WHERE id = $1")
                .bind(ws)
                .fetch_one(&self.pool)
                .await?;
        let Some(organization_id) = organization_id else {
            return Ok(BTreeMap::new());
        };
        let own_style = sqlx::query_scalar::<_, String>(
            "SELECT value FROM tenant_settings WHERE workspace_id = $1 AND key = 'act_style'",
        )
        .bind(ws)
        .fetch_optional(&self.pool)
        .await?
        .map(|style| normalize_style(&style))
        .filter(|style| !style.is_empty());
        let Some(own_style) = own_style else {
            return Ok(BTreeMap::new());
        };

        let sibling_rows = sqlx::query_as::<_, (Uuid, String)>(
            r#"
            SELECT member.id, setting.value
            FROM workspaces AS member
            JOIN tenant_settings AS setting
              ON setting.workspace_id = member.id AND setting.key = 'act_style'
            WHERE member.organization_id = $1 AND member.id <> $2
            "#,
        )
        .bind(organization_id)
        .bind(ws)
        .fetch_all(&self.pool)
        .await?;
        let sibling_ids: Vec<Uuid> = sibling_rows
            .into_iter()
            .filter(|(_, style)| normalize_style(style) == own_style)
            .map(|(id, _)| id)
            .collect();
        if sibling_ids.is_empty() {
            return Ok(BTreeMap::new());
        }

        let rows = sqlx::query_as::<_, (String, i64)>(
            r#"
            SELECT s.format_key, count(o.id) AS produced
            FROM content_suggestions AS s
            JOIN suggestion_outcomes AS o
              ON o.workspace_id = s.workspace_id AND o.suggestion_id = s.id
            WHERE s.workspace_id = ANY($1)
              AND s.format_key IS NOT NULL
              AND o.outcome IN ('done', 'done_differently')
            GROUP BY s.format_key
            "#,
        )
        .bind(&sibling_ids)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(key, produced)| (key, u32::try_from(produced).unwrap_or(u32::MAX)))
            .collect())
    }

    /// One ranking pass: gather inputs, rank, persist the survivors as
    /// `raised` suggestions. Returns the rows written — an empty vec is a
    /// truthful "nothing worth the band's time today".
    ///
    /// Format keys the band declined inside [`TASTE_COOLDOWN_DAYS`] — the
    /// "not for us" the ranker honours. Only `declined` counts: `expired`
    /// is timing and `done_differently` is a version of yes. Bespoke
    /// concepts carry no catalogue key, so nothing here can suppress them
    /// — recorded, but not a taste signal the engine can act on. The read
    /// runs inside the caller's transaction so a decline committed before
    /// the lock applies to this sweep.
    pub(crate) async fn declined_format_keys(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        workspace_id: WorkspaceId,
    ) -> Result<BTreeSet<String>> {
        let keys = sqlx::query_scalar::<_, String>(
            r#"
            SELECT DISTINCT s.format_key
            FROM suggestion_outcomes AS o
            JOIN content_suggestions AS s
              ON s.workspace_id = o.workspace_id AND s.id = o.suggestion_id
            WHERE o.workspace_id = $1 AND o.outcome = 'declined'
              AND o.resolved_at >= now() - make_interval(days => $2)
              AND s.format_key IS NOT NULL
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(i32::try_from(TASTE_COOLDOWN_DAYS).unwrap_or(i32::MAX))
        .fetch_all(&mut **tx)
        .await?;
        Ok(keys.into_iter().collect())
    }

    /// The input reads happen before the transaction — they are a loose
    /// snapshot of slowly-changing state (formats, trends, reach), and
    /// holding a connection across them would starve a one-connection
    /// pool. The correctness-critical read — which formats are already
    /// open — runs *inside* the transaction under the advisory lock, so
    /// two concurrent passes cannot interleave into a double-raise. The
    /// lock key differs from the trend refresh's on purpose: a suggestion
    /// pass only reads `content_trends` after that writer commits,
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
        let (suggestion_counts, outcome_counts, produced_counts) =
            self.format_history(&self.pool, workspace_id).await?;
        let format_yield = self.format_yields(&self.pool, workspace_id).await?;
        let sibling_produced = self.sibling_produced_counts(workspace_id).await?;

        // §4b-4 — a concept that has been offered STALE_ATTEMPT_LIMIT
        // times and never produced has had its chances: it retires on its
        // own record, no operator verdict needed. Retired means gone —
        // the ranker drops it before scoring, so the tail's "other
        // feasible" names never resurrect it either.
        let retired_format_keys: BTreeSet<String> = suggestion_counts
            .iter()
            .filter(|(key, count)| {
                is_stale(**count, produced_counts.get(*key).copied().unwrap_or(0))
            })
            .map(|(key, _)| key.clone())
            .collect();

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
        // are the band's commitment, not an ask — they do not lapse on the
        // ask's `expires_at`. They lapse on the beat's day instead: an
        // approved suggestion whose `suggested_before` passed without a
        // report is as dead as a lapsed ask, and it holds a queue slot the
        // same way. 'expired' is the honest label — the window closed;
        // whether the band played the beat anyway is unmeasured, and the
        // reason says so rather than guess a done.
        let lapsed = sqlx::query_scalar::<_, Uuid>(
            r#"
            UPDATE content_suggestions
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
                INSERT INTO suggestion_outcomes (
                    workspace_id, suggestion_id, outcome, decided_by, reason
                ) VALUES ($1, $2, 'expired', 'system', 'the window this beat was for has passed')
                "#,
            )
            .bind(ws)
            .bind(suggestion_id)
            .execute(&mut *tx)
            .await?;
        }
        let unreported = sqlx::query_scalar::<_, Uuid>(
            r#"
            UPDATE content_suggestions
            SET status = 'expired', updated_at = now()
            WHERE workspace_id = $1 AND status = 'approved'
              AND suggested_before IS NOT NULL AND suggested_before < $2
            RETURNING id
            "#,
        )
        .bind(ws)
        .bind(today)
        .fetch_all(&mut *tx)
        .await?;
        for suggestion_id in unreported {
            sqlx::query(
                r#"
                INSERT INTO suggestion_outcomes (
                    workspace_id, suggestion_id, outcome, decided_by, reason
                ) VALUES ($1, $2, 'expired', 'system', 'committed, but the beat''s day passed without a report — unmeasured')
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
            SELECT format_key FROM content_suggestions
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

        let declined_format_keys = self.declined_format_keys(&mut tx, workspace_id).await?;

        let ranked = rank_suggestions(&RankingInputs {
            formats: &formats,
            profile: &profile,
            trends: &trends,
            production: &production,
            open_format_keys: &open_format_keys,
            declined_format_keys: &declined_format_keys,
            retired_format_keys: &retired_format_keys,
            arc_format_keys: &arc_keys,
            outcome_counts: &outcome_counts,
            format_yield: &format_yield,
            suggestion_counts: &suggestion_counts,
            sibling_produced: &sibling_produced,
            reach: &reach,
            weights: Default::default(),
            today,
        });

        // Rank, then cut — and name the tail out loud so "these two carry
        // most of it" is a claim the operator can check: the count and the
        // concept names ride in each raised row's reason and evidence, so
        // asking for the rest is possible from the row itself.
        let (top, tail) = ranked.split_at(headroom.min(ranked.len()));
        let tail_names: Vec<&str> = tail.iter().map(|s| s.concept.as_str()).collect();
        let tail_clause = if tail.is_empty() {
            String::new()
        } else {
            format!(
                " — {} other feasible format{} this pass; these carried the most ({})",
                tail.len(),
                if tail.len() == 1 { "" } else { "s" },
                tail_names.join(", ")
            )
        };

        let mut raised = Vec::with_capacity(top.len());
        for scored in top {
            let mut reason = scored.reason.clone();
            reason.push_str(&tail_clause);
            let mut evidence = scored.evidence.clone();
            if let Some(object) = evidence.as_object_mut() {
                object.insert(
                    "tail".to_owned(),
                    json!({
                        "count": tail.len(),
                        "concepts": tail_names,
                    }),
                );
            } else {
                debug_assert!(
                    false,
                    "ranked suggestion evidence is always a JSON object; the tail would be lost"
                );
            }
            // A covered suggestion's window is the production day itself —
            // after it passes, the near-free price is a lie and the row
            // expires.
            let expires_at = scored
                .suggested_before
                .map(|day| day.midnight().assume_utc() + time::Duration::days(1));
            let row = sqlx::query_as::<_, SuggestionRow>(
                r#"
                INSERT INTO content_suggestions (
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
            .bind(&reason)
            .bind(&evidence)
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

/// The declared-taste comparison, one spelling for every caller: lowercase,
/// whitespace-folded. Two acts that wrote the same words differently still
/// said the same thing; an act that never said anything matches nothing.
fn normalize_style(style: &str) -> String {
    style
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}
