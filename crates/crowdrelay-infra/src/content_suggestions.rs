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

/// A content piece gets a full fortnight to convert before its observed fan
/// yield becomes a training sample. Fresh posts are unknown, not zero.
const FORMAT_YIELD_WINDOW_DAYS: i32 = 14;

fn fold_format_yield_sample(
    yields: &mut BTreeMap<String, FormatYield>,
    format_key: String,
    new_fans: f64,
) {
    let entry = yields.entry(format_key).or_insert(FormatYield {
        measured_fans_ema: new_fans,
        measured: 0,
    });
    entry.measured_fans_ema = if entry.measured == 0 {
        new_fans
    } else {
        YIELD_EMA_ALPHA * new_fans + (1.0 - YIELD_EMA_ALPHA) * entry.measured_fans_ema
    };
    entry.measured = entry.measured.saturating_add(1);
}

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
    /// admitted-or-promoted press-route candidates, roster audiences that
    /// have an ACTIVE amplification consent into this workspace and at least
    /// one reachable fan today, and fans reachable under a marketing consent.
    ///
    /// A confirmed `peers` row is research, not distribution authority.
    /// This distinction is load-bearing: observing a good band must never
    /// become "promote their content" or "their audience will carry ours".
    /// The only peer audience that counts as reach is an explicit, revocable,
    /// capped portfolio edge in the direction peer -> this workspace.
    ///
    /// `fan_consents` is append-only, so both local and portfolio reach read
    /// each fan's *latest* marketing row: a grant followed by a withdrawal is
    /// not consent.
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
                    (
                        SELECT array_agg(DISTINCT audience_owner.name ORDER BY audience_owner.name)
                        FROM amplification_consents AS edge
                        JOIN workspaces AS audience_owner
                          ON audience_owner.id = edge.from_workspace_id
                        WHERE edge.to_workspace_id = $1
                          AND edge.status = 'active'
                          -- Event crossbill consent is scoped to that event,
                          -- not a standing right to use the act's audience for
                          -- generic content ideas.
                          AND edge.purpose IN ('cross_promote','release_feature')
                          -- A spent monthly edge is not reach available now.
                          AND (
                              SELECT count(DISTINCT ledger.campaign_reference)
                              FROM amplification_deliveries AS ledger
                              WHERE ledger.consent_id = edge.id
                                AND ledger.delivered_at >= date_trunc('month', now())
                          ) < edge.max_campaigns_per_month
                          -- Name the audience only when at least one fan could
                          -- actually receive an amplification today. "Active
                          -- edge, zero eligible humans" is measured zero reach,
                          -- not a promise.
                          AND EXISTS (
                              SELECT 1
                              FROM fans AS peer_fan
                              WHERE peer_fan.workspace_id = edge.from_workspace_id
                                AND peer_fan.status = 'active'
                                AND EXISTS (
                                    SELECT 1 FROM fan_consents AS consent
                                    WHERE consent.workspace_id = peer_fan.workspace_id
                                      AND consent.fan_id = peer_fan.id
                                      AND consent.purpose = 'marketing'
                                      AND consent.granted
                                      AND consent.id = (
                                          SELECT newest.id
                                          FROM fan_consents AS newest
                                          WHERE newest.workspace_id = peer_fan.workspace_id
                                            AND newest.fan_id = peer_fan.id
                                            AND newest.purpose = 'marketing'
                                          ORDER BY newest.recorded_at DESC, newest.id DESC
                                          LIMIT 1
                                      )
                                )
                                AND NOT EXISTS (
                                    SELECT 1
                                    FROM amplification_deliveries AS recent
                                    WHERE recent.consent_id = edge.id
                                      AND recent.fan_id = peer_fan.id
                                      AND recent.delivered_at >
                                          now() - make_interval(days => edge.cooldown_days::int)
                                )
                          )
                    ),
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

    /// What this band's produced formats actually earned in new fans.
    ///
    /// First-party provenance is authoritative whenever the format has at
    /// least one mature, published source: each source becomes one sample
    /// after a fourteen-day conversion window, including an explicit zero
    /// when it acquired nobody. The path is fully observed:
    ///
    /// content source -> posting action -> tracked click -> conversion
    /// provenance -> fan.
    ///
    /// A fresh piece is not a zero — it stays out until the full window has
    /// elapsed. Manual `suggestion_outcomes.results.new_fans` survives as a
    /// legacy/uninstrumented fallback only for formats with no first-party
    /// samples. Once CrowdRelay can measure a format itself, self-report never
    /// double-counts or overrides the observed truth.
    pub(crate) async fn format_yields(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<BTreeMap<String, FormatYield>> {
        let ws = workspace_id.into_uuid();

        // One row per mature content source. A LEFT JOIN is load-bearing:
        // published content that converted zero fans is a real training sample,
        // unlike fresh/unpublished content, which never enters this CTE.
        let observed_rows = sqlx::query_as::<_, (String, f64)>(
            r#"
            WITH post_receipts AS (
                SELECT workspace_id, action_id, posted_at
                FROM community_posts
                WHERE workspace_id = $1 AND posted_at IS NOT NULL
                UNION ALL
                SELECT workspace_id, action_id, posted_at
                FROM social_posts
                WHERE workspace_id = $1 AND posted_at IS NOT NULL
                UNION ALL
                SELECT workspace_id, action_id, posted_at
                FROM telegram_posts
                WHERE workspace_id = $1 AND posted_at IS NOT NULL
                UNION ALL
                SELECT workspace_id, action_id, posted_at
                FROM discord_posts
                WHERE workspace_id = $1 AND posted_at IS NOT NULL
            ),
            published_sources AS (
                SELECT source.id AS source_id,
                       source.format_key,
                       min(post.posted_at) AS published_at
                FROM content_sources AS source
                JOIN autopilot_actions AS action
                  ON action.workspace_id = source.workspace_id
                 AND lower(COALESCE(action.payload->>'source_id', action.payload->'draft'->>'source_id')) = source.id::text
                JOIN post_receipts AS post
                  ON post.workspace_id = action.workspace_id
                 AND post.action_id = action.id
                WHERE source.workspace_id = $1
                  AND source.format_key IS NOT NULL
                GROUP BY source.id, source.format_key
            ),
            source_actions AS (
                SELECT source.source_id,
                       source.format_key,
                       source.published_at,
                       action.id AS action_id
                FROM published_sources AS source
                JOIN autopilot_actions AS action
                  ON action.workspace_id = $1
                 AND lower(COALESCE(action.payload->>'source_id', action.payload->'draft'->>'source_id')) = source.source_id::text
                WHERE source.published_at <=
                      now() - make_interval(days => $2)
            )
            SELECT source.format_key,
                   count(DISTINCT provenance.fan_id)::double precision AS new_fans
            FROM source_actions AS source
            LEFT JOIN fan_provenance_events AS provenance
              ON provenance.workspace_id = $1
             AND provenance.action_id = source.action_id
             AND provenance.event_kind = 'conversion'
             AND provenance.attribution_method = 'last_tracked_click'
             AND provenance.format_key = source.format_key
             AND provenance.occurred_at >= source.published_at
             AND provenance.occurred_at <
                 source.published_at + make_interval(days => $2)
            GROUP BY source.source_id, source.format_key, source.published_at
            ORDER BY source.format_key, source.published_at, source.source_id
            "#,
        )
        .bind(ws)
        .bind(FORMAT_YIELD_WINDOW_DAYS)
        .fetch_all(&self.pool)
        .await?;

        let first_party_formats: BTreeSet<String> = observed_rows
            .iter()
            .map(|(format_key, _)| format_key.clone())
            .collect();
        let mut format_yield: BTreeMap<String, FormatYield> = BTreeMap::new();
        for (format_key, new_fans) in observed_rows {
            fold_format_yield_sample(&mut format_yield, format_key, new_fans);
        }

        // Backward-compatible fallback for content that predates source-level
        // instrumentation or was honestly filed without a trackable source.
        // `jsonb_typeof` guards the cast: malformed operator JSON cannot
        // abort the whole ranking pass.
        let reported_rows = sqlx::query_as::<_, (String, f64)>(
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
        .bind(ws)
        .fetch_all(&self.pool)
        .await?;
        for (format_key, new_fans) in reported_rows {
            if !first_party_formats.contains(&format_key) {
                fold_format_yield_sample(&mut format_yield, format_key, new_fans);
            }
        }

        Ok(format_yield)
    }

    /// 5.6 — shared learning, respecting per-band taste.
    ///
    /// Same-organisation acts only pool when their declared `act_style`
    /// normalises to the target act's style. The evidence is first-party fan
    /// yield per mature content source — not "another band made this", but
    /// "another similar band made this and here is how many fans it earned".
    ///
    /// Zero-conversion mature pieces remain samples. Fresh pieces do not.
    /// No organisation or no declared style returns an empty prior rather
    /// than guessing similarity from genre, name, audience or embeddings.
    async fn sibling_format_yields(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<BTreeMap<String, FormatYield>> {
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

        let rows = sqlx::query_as::<_, (String, f64)>(
            r#"
            WITH post_receipts AS (
                SELECT workspace_id, action_id, posted_at
                FROM community_posts
                WHERE workspace_id = ANY($1) AND posted_at IS NOT NULL
                UNION ALL
                SELECT workspace_id, action_id, posted_at
                FROM social_posts
                WHERE workspace_id = ANY($1) AND posted_at IS NOT NULL
                UNION ALL
                SELECT workspace_id, action_id, posted_at
                FROM telegram_posts
                WHERE workspace_id = ANY($1) AND posted_at IS NOT NULL
                UNION ALL
                SELECT workspace_id, action_id, posted_at
                FROM discord_posts
                WHERE workspace_id = ANY($1) AND posted_at IS NOT NULL
            ),
            published_sources AS (
                SELECT source.workspace_id,
                       source.id AS source_id,
                       source.format_key,
                       min(post.posted_at) AS published_at
                FROM content_sources AS source
                JOIN autopilot_actions AS action
                  ON action.workspace_id = source.workspace_id
                 AND lower(COALESCE(action.payload->>'source_id', action.payload->'draft'->>'source_id')) = source.id::text
                JOIN post_receipts AS post
                  ON post.workspace_id = action.workspace_id
                 AND post.action_id = action.id
                WHERE source.workspace_id = ANY($1)
                  AND source.format_key IS NOT NULL
                GROUP BY source.workspace_id, source.id, source.format_key
            ),
            source_actions AS (
                SELECT source.workspace_id,
                       source.source_id,
                       source.format_key,
                       source.published_at,
                       action.id AS action_id
                FROM published_sources AS source
                JOIN autopilot_actions AS action
                  ON action.workspace_id = source.workspace_id
                 AND lower(COALESCE(action.payload->>'source_id', action.payload->'draft'->>'source_id')) = source.source_id::text
                WHERE source.published_at <=
                      now() - make_interval(days => $2)
            )
            SELECT source.format_key,
                   count(DISTINCT provenance.fan_id)::double precision AS new_fans
            FROM source_actions AS source
            LEFT JOIN fan_provenance_events AS provenance
              ON provenance.workspace_id = source.workspace_id
             AND provenance.action_id = source.action_id
             AND provenance.event_kind = 'conversion'
             AND provenance.attribution_method = 'last_tracked_click'
             AND provenance.format_key = source.format_key
             AND provenance.occurred_at >= source.published_at
             AND provenance.occurred_at <
                 source.published_at + make_interval(days => $2)
            GROUP BY source.workspace_id, source.source_id,
                     source.format_key, source.published_at
            ORDER BY source.format_key, source.published_at,
                     source.workspace_id, source.source_id
            "#,
        )
        .bind(&sibling_ids)
        .bind(FORMAT_YIELD_WINDOW_DAYS)
        .fetch_all(&self.pool)
        .await?;

        let mut yields = BTreeMap::new();
        for (format_key, new_fans) in rows {
            fold_format_yield_sample(&mut yields, format_key, new_fans);
        }
        Ok(yields)
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
        let format_yield = self.format_yields(workspace_id).await?;
        let sibling_format_yield = self.sibling_format_yields(workspace_id).await?;

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
        // Each expiry and its outcome row commit in one statement: the
        // UPDATE's RETURNING feeds the INSERT directly, instead of a round
        // trip per expired suggestion.
        sqlx::query(
            r#"
            WITH lapsed AS (
                UPDATE content_suggestions
                SET status = 'expired', updated_at = now()
                WHERE workspace_id = $1 AND status = 'raised'
                  AND expires_at IS NOT NULL AND expires_at <= now()
                RETURNING id
            )
            INSERT INTO suggestion_outcomes (
                workspace_id, suggestion_id, outcome, decided_by, reason
            )
            SELECT $1, lapsed.id, 'expired', 'system', 'the window this beat was for has passed'
            FROM lapsed
            "#,
        )
        .bind(ws)
        .execute(&mut *tx)
        .await?;
        // Reach authority is revocable. If a raised collaboration promise
        // names a peer audience that is no longer in the live reach snapshot,
        // the ask is no longer executable. Heal the whole active chain here:
        // suggestion -> awaiting approval action -> crew handoff/reminder.
        //
        // Decisions remain immutable audit evidence, and approved suggestions
        // are not touched: once a person committed to the beat, withdrawing it
        // is another human decision rather than silent system cleanup.
        sqlx::query(
            r#"
            WITH stale_peer_reach AS (
                UPDATE content_suggestions AS suggestion
                SET status = 'expired', updated_at = now()
                WHERE suggestion.workspace_id = $1
                  AND suggestion.status = 'raised'
                  AND jsonb_typeof(suggestion.distribution_promise->'peer_audience') = 'array'
                  AND EXISTS (
                      SELECT 1
                      FROM jsonb_array_elements_text(
                          suggestion.distribution_promise->'peer_audience'
                      ) AS promised(peer_name)
                      WHERE NOT (promised.peer_name = ANY($2::text[]))
                  )
                RETURNING suggestion.workspace_id, suggestion.id
            ),
            cancelled_actions AS (
                UPDATE autopilot_actions AS action
                SET status = 'cancelled',
                    finished_at = now(),
                    last_error_kind = 'peer_reach_unavailable',
                    idempotency_key =
                        action.idempotency_key || ':peer-reach:' || action.id::text
                FROM stale_peer_reach AS stale
                WHERE action.workspace_id = stale.workspace_id
                  AND action.subject_kind = 'content_suggestion'
                  AND action.subject_id = stale.id
                  AND action.status = 'awaiting_approval'
                RETURNING action.workspace_id, action.id
            ),
            cancelled_assignments AS (
                UPDATE team_assignments AS assignment
                SET status = 'cancelled',
                    completed_at = NULL,
                    next_reminder_at = NULL,
                    updated_at = now()
                FROM cancelled_actions AS action
                WHERE assignment.workspace_id = action.workspace_id
                  AND assignment.action_id = action.id
                  AND assignment.status = 'open'
                RETURNING assignment.id
            )
            INSERT INTO suggestion_outcomes (
                workspace_id, suggestion_id, outcome, decided_by, reason
            )
            SELECT stale_peer_reach.workspace_id, stale_peer_reach.id,
                   'expired', 'system',
                   'peer audience is no longer an executable consented route'
            FROM stale_peer_reach
            "#,
        )
        .bind(ws)
        .bind(&reach.peers)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            WITH unreported AS (
                UPDATE content_suggestions
                SET status = 'expired', updated_at = now()
                WHERE workspace_id = $1 AND status = 'approved'
                  AND suggested_before IS NOT NULL AND suggested_before < $2
                RETURNING id
            )
            INSERT INTO suggestion_outcomes (
                workspace_id, suggestion_id, outcome, decided_by, reason
            )
            SELECT $1, unreported.id, 'expired', 'system', 'committed, but the beat''s day passed without a report — unmeasured'
            FROM unreported
            "#,
        )
        .bind(ws)
        .bind(today)
        .execute(&mut *tx)
        .await?;

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
            sibling_format_yield: &sibling_format_yield,
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
