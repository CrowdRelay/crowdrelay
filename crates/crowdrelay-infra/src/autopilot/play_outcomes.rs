//! Reading what a play did, without ever reading more than is there.
//!
//! Two rules shape every query in this file.
//!
//! The baseline is frozen in the transaction that creates the play, because a
//! baseline computed later is read from a series the play has already moved.
//! Everything after that is a comparison against a number nobody can revise.
//!
//! And a gap stays a gap. A missing series, two series answering to the same
//! metric, and an absent first-party join key are three different facts, and
//! each comes back as itself rather than as a zero, a guess, or the other
//! claim's number.

use super::*;

/// How far back the pre-play and post-play rates are derived from. Matches the
/// window `compute_trend` reasons over, so both ends of the comparison are the
/// same kind of number.
const TREND_WINDOW_DAYS: i32 = 35;

#[derive(sqlx::FromRow)]
struct MetricPointRow {
    captured_at: OffsetDateTime,
    value: i64,
}

#[derive(sqlx::FromRow)]
struct PlayOutcomeRow {
    id: Uuid,
    play_id: Uuid,
    play_kind: String,
    claim: String,
    success_metric_platform: String,
    success_metric_key: String,
    baseline_value: Option<i64>,
    baseline_milli_per_day: Option<i64>,
    window_start: OffsetDateTime,
    window_end: OffsetDateTime,
    attempt_count: i32,
}

/// One series' state at a moment, or the reason there is no such thing.
pub(super) struct SeriesReading {
    pub direction: MetricDirection,
    pub value: Option<i64>,
    pub milli_per_day: Option<i64>,
    /// More than one live series answers to this metric. Neither picking one
    /// nor adding them is right, so the caller is told and refuses.
    pub ambiguous: bool,
}

/// Reads the series a play names as its success metric, as of `at`.
///
/// `at` is the moment the number should describe, not the moment we happen to
/// be looking. A worker that runs a day late must still read the play's window
/// rather than the day after it.
pub(super) async fn read_series(
    executor: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    platform: &str,
    metric_key: &str,
    at: OffsetDateTime,
) -> Result<SeriesReading, RepositoryError> {
    // A play is a workspace-level campaign, so where the workspace declares a
    // workspace-level series for the metric, that is the series. A per-city or
    // per-source breakdown of the same key is a different question, not a
    // second answer to this one — and treating it as one made every metric with
    // a breakdown permanently unmeasurable.
    //
    // The fallback matters just as much: `bandsintown/trackers` exists *only*
    // per event source, deliberately, because a workspace may sync two artists.
    // One source resolves; two stay ambiguous, which is the honest answer.
    let series = sqlx::query_as::<_, (Uuid, String)>(
        r#"
        WITH matching AS (
            SELECT series.id, series.direction, (series.subject_kind IS NULL) AS workspace_level
            FROM viryaos_growth_metric_series AS series
            WHERE series.workspace_id = $1
              AND series.platform = $2
              AND series.metric_key = $3
              AND series.active
        )
        SELECT id, direction
        FROM matching
        WHERE workspace_level = EXISTS (SELECT 1 FROM matching WHERE workspace_level)
        ORDER BY id
        LIMIT 2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(platform)
    .bind(metric_key)
    .fetch_all(&mut **executor)
    .await
    .map_err(map_sqlx)?;

    let mut series = series.into_iter();
    let Some((series_id, direction)) = series.next() else {
        return Ok(SeriesReading {
            direction: MetricDirection::HigherIsBetter,
            value: None,
            milli_per_day: None,
            ambiguous: false,
        });
    };
    let direction = MetricDirection::parse(&direction).ok_or(RepositoryError::Unexpected)?;
    if series.next().is_some() {
        return Ok(SeriesReading {
            direction,
            value: None,
            milli_per_day: None,
            ambiguous: true,
        });
    }

    let points = sqlx::query_as::<_, MetricPointRow>(
        r#"
        SELECT point.captured_at, point.value
        FROM viryaos_growth_metric_points AS point
        WHERE point.workspace_id = $1
          AND point.series_id = $2
          AND point.captured_at <= $3
          AND point.captured_at >= $3 - make_interval(days => $4::int)
        ORDER BY point.captured_at
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(series_id)
    .bind(at)
    .bind(TREND_WINDOW_DAYS)
    .fetch_all(&mut **executor)
    .await
    .map_err(map_sqlx)?;
    let points: Vec<MetricPoint> = points
        .into_iter()
        .map(|row| MetricPoint {
            captured_at: row.captured_at,
            value: row.value,
        })
        .collect();
    let trend = compute_trend(&points, at);
    Ok(SeriesReading {
        direction,
        value: trend.map(|trend| trend.latest_value),
        milli_per_day: trend.and_then(|trend| trend.velocity_milli_per_day),
        ambiguous: false,
    })
}

/// Opens both claims for a play, with the baseline frozen as it stands now.
///
/// Called inside `start_play`, in the same transaction. A play that existed
/// without its baseline would be a campaign nobody could ever measure, and the
/// window to capture one closes the moment the first step runs.
pub(super) async fn open_play_outcomes(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    play_id: Uuid,
    start: &PlayStart,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let baseline = read_series(
        transaction,
        workspace_id,
        start.success_metric_platform,
        start.success_metric_key,
        now,
    )
    .await?;
    for claim in PlayClaim::all() {
        sqlx::query(
            r#"
            INSERT INTO viryaos_play_outcomes (
                workspace_id, play_id, claim,
                success_metric_platform, success_metric_key,
                baseline_captured_at, baseline_value, baseline_milli_per_day,
                window_start, window_end, available_at
            )
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$6,$9,$9)
            ON CONFLICT (workspace_id, play_id, claim) DO NOTHING
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(play_id)
        .bind(claim.as_str())
        .bind(start.success_metric_platform)
        .bind(start.success_metric_key)
        .bind(now)
        .bind(baseline.value)
        .bind(baseline.milli_per_day)
        .bind(start.measurement_window_end)
        .execute(&mut **transaction)
        .await
        .map_err(map_sqlx)?;
    }
    Ok(())
}

impl PostgresAutopilotRepository {
    pub(super) async fn claim_due_play_outcomes_impl(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
        now: OffsetDateTime,
    ) -> Result<Vec<ClaimedPlayOutcome>, RepositoryError> {
        self.bounded(async {
            let rows = sqlx::query_as::<_, PlayOutcomeRow>(
                r#"
                WITH due AS (
                    SELECT outcome.id
                    FROM viryaos_play_outcomes AS outcome
                    WHERE outcome.workspace_id = $1
                      AND outcome.status = 'pending'
                      AND outcome.available_at <= $2
                      -- The window has to have closed. Reading it early would
                      -- measure a campaign that is still running and call the
                      -- half-result its effect.
                      AND outcome.window_end <= $2
                    ORDER BY outcome.available_at
                    LIMIT $3
                    FOR UPDATE SKIP LOCKED
                )
                UPDATE viryaos_play_outcomes AS outcome
                SET status = 'processing',
                    attempt_count = outcome.attempt_count + 1,
                    started_at = $2
                FROM due
                WHERE outcome.workspace_id = $1 AND outcome.id = due.id
                RETURNING
                    outcome.id, outcome.play_id,
                    (SELECT play.play_kind FROM viryaos_plays AS play
                      WHERE play.workspace_id = outcome.workspace_id
                        AND play.id = outcome.play_id) AS play_kind,
                    outcome.claim,
                    outcome.success_metric_platform, outcome.success_metric_key,
                    outcome.baseline_value, outcome.baseline_milli_per_day,
                    outcome.window_start, outcome.window_end, outcome.attempt_count
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .bind(i64::from(limit))
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
            rows.into_iter()
                .map(|row| {
                    Ok(ClaimedPlayOutcome {
                        id: row.id,
                        play_id: PlayId::from_uuid(row.play_id),
                        kind: PlayKind::parse(&row.play_kind).ok_or(RepositoryError::Unexpected)?,
                        claim: PlayClaim::parse(&row.claim).ok_or(RepositoryError::Unexpected)?,
                        success_metric_platform: row.success_metric_platform,
                        success_metric_key: row.success_metric_key,
                        baseline_value: row.baseline_value,
                        baseline_milli_per_day: row.baseline_milli_per_day,
                        window_start: row.window_start,
                        window_end: row.window_end,
                        attempt_number: u32::try_from(row.attempt_count)
                            .map_err(|_| RepositoryError::Unexpected)?,
                    })
                })
                .collect()
        })
        .await
    }

    pub(super) async fn observe_play_outcome_impl(
        &self,
        workspace_id: WorkspaceId,
        outcome: &ClaimedPlayOutcome,
        _now: OffsetDateTime,
    ) -> Result<PlayOutcomeObservation, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            // Read the series as of the window's end, not as of the clock. A
            // worker running two days late must still describe the play's own
            // window rather than everything that happened since.
            let series = read_series(
                &mut transaction,
                workspace_id,
                &outcome.success_metric_platform,
                &outcome.success_metric_key,
                outcome.window_end,
            )
            .await?;
            // Reach comes from the delivered-recipient rows, not from the
            // actions the play created. An action that was queued and never
            // sent reached nobody, and counting it would give every number here
            // a denominator larger than the truth.
            let recipients_reached = sqlx::query_scalar::<_, i64>(
                r#"
                SELECT count(*)::bigint
                FROM viryaos_play_step_recipients AS recipient
                JOIN viryaos_play_steps AS step
                  ON step.workspace_id = recipient.workspace_id
                 AND step.id = recipient.step_id
                WHERE recipient.workspace_id = $1
                  AND step.play_id = $2
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(outcome.play_id.into_uuid())
            .fetch_one(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(PlayOutcomeObservation {
                observed_at: outcome.window_end,
                observed_value: series.value,
                observed_milli_per_day: series.milli_per_day,
                recipients_reached: u32::try_from(recipients_reached)
                    .map_err(|_| RepositoryError::Unexpected)?,
                // No play mints a tracked link yet, so no click can be joined
                // to one. `None` says exactly that. Returning `Some(0)` would
                // claim we looked and found nothing, which is the difference
                // between a measured null result and an unanswerable question.
                attributed_clicks: None,
                direction: series.direction,
                ambiguous_series: series.ambiguous,
            })
        })
        .await
    }

    pub(super) async fn complete_play_outcome_impl(
        &self,
        workspace_id: WorkspaceId,
        outcome: &ClaimedPlayOutcome,
        observation: &PlayOutcomeObservation,
        verdict: PlayOutcomeVerdict,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError> {
        self.bounded(async {
            let (evidence, reason, assessment, delta) = match verdict {
                PlayOutcomeVerdict::Measured {
                    assessment,
                    delta_basis_points,
                } => (
                    "measured",
                    None,
                    assessment.map(effect_assessment_str),
                    delta_basis_points,
                ),
                PlayOutcomeVerdict::Insufficient { reason } => {
                    ("insufficient", Some(reason.as_str()), None, None)
                }
            };
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let updated = sqlx::query(
                r#"
                UPDATE viryaos_play_outcomes
                SET status = 'succeeded',
                    finished_at = $3,
                    observed_at = $4,
                    observed_value = $5,
                    observed_milli_per_day = $6,
                    recipients_reached = $7,
                    evidence = $8,
                    evidence_reason = $9,
                    effect_assessment = $10,
                    delta_basis_points = $11,
                    last_error_kind = NULL
                WHERE workspace_id = $1 AND id = $2 AND status = 'processing'
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(outcome.id)
            .bind(now)
            .bind(observation.observed_at)
            .bind(observation.observed_value)
            .bind(observation.observed_milli_per_day)
            .bind(i32::try_from(observation.recipients_reached).unwrap_or(i32::MAX))
            .bind(evidence)
            .bind(reason)
            .bind(assessment)
            .bind(delta)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            if updated.rows_affected() != 1 {
                return Err(RepositoryError::Conflict);
            }
            // The record moves with the outcome or not at all. Two writes would
            // let a crash leave a play scored and unlearned from, and the
            // difference is invisible afterwards.
            if outcome.claim == PlayClaim::Correlational {
                record_play_outcome(
                    &mut transaction,
                    workspace_id,
                    outcome.kind,
                    match verdict {
                        PlayOutcomeVerdict::Measured { assessment, .. } => assessment,
                        PlayOutcomeVerdict::Insufficient { .. } => None,
                    },
                )
                .await?;
            }
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(())
        })
        .await
    }

    pub(super) async fn fail_play_outcome_impl(
        &self,
        workspace_id: WorkspaceId,
        outcome_id: Uuid,
        error_kind: &'static str,
        retryable: bool,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError> {
        self.bounded(async {
            // A failed reading never writes evidence. The schema enforces the
            // same thing from the other side: only a succeeded row may carry
            // one, so a retry cannot find a half-written verdict.
            sqlx::query(
                r#"
                UPDATE viryaos_play_outcomes
                SET status = CASE
                        WHEN $5 AND attempt_count < 5 THEN 'pending'
                        ELSE 'failed'
                    END,
                    available_at = CASE
                        WHEN $5 AND attempt_count < 5 THEN $3 + INTERVAL '6 hours'
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
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(outcome_id)
            .bind(now)
            .bind(error_kind)
            .bind(retryable)
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
            Ok(())
        })
        .await
    }
}

#[derive(sqlx::FromRow)]
struct PlayLedgerRow {
    play_id: Uuid,
    play_kind: String,
    anchor_kind: String,
    anchor_id: Uuid,
    anchor_at: OffsetDateTime,
    hypothesis: String,
    state: String,
    started_at: OffsetDateTime,
    completed_at: Option<OffsetDateTime>,
    steps_total: i64,
    steps_settled: i64,
    steps_skipped: i64,
    recipients_reached: i64,
}

#[derive(sqlx::FromRow)]
struct PlayClaimRow {
    play_id: Uuid,
    claim: String,
    success_metric_platform: String,
    success_metric_key: String,
    window_start: OffsetDateTime,
    window_end: OffsetDateTime,
    status: String,
    evidence: Option<String>,
    evidence_reason: Option<String>,
    effect_assessment: Option<String>,
    delta_basis_points: Option<i32>,
    baseline_milli_per_day: Option<i64>,
    observed_milli_per_day: Option<i64>,
    recipients_reached: Option<i32>,
}

#[async_trait]
impl AutopilotPlayLedgerRepository for PostgresAutopilotRepository {
    async fn load_play_ledger(
        &self,
        workspace_id: WorkspaceId,
        _now: OffsetDateTime,
    ) -> Result<PlayLedger, RepositoryError> {
        self.bounded(async {
            let plays = sqlx::query_as::<_, PlayLedgerRow>(
                r#"
                SELECT
                    play.id AS play_id,
                    play.play_kind,
                    play.anchor_kind,
                    play.anchor_id,
                    play.anchor_at,
                    play.hypothesis,
                    play.state,
                    play.started_at,
                    play.completed_at,
                    (SELECT count(*)::bigint FROM viryaos_play_steps AS step
                      WHERE step.workspace_id = play.workspace_id
                        AND step.play_id = play.id) AS steps_total,
                    (SELECT count(*)::bigint FROM viryaos_play_steps AS step
                      WHERE step.workspace_id = play.workspace_id
                        AND step.play_id = play.id
                        AND step.settled_at IS NOT NULL) AS steps_settled,
                    -- Skipped, not merely settled. A step that was delivered and
                    -- one nobody could send both settle, and reporting them as
                    -- one number is how a campaign that did nothing reads as a
                    -- campaign that ran.
                    (SELECT count(*)::bigint FROM viryaos_play_steps AS step
                      WHERE step.workspace_id = play.workspace_id
                        AND step.play_id = play.id
                        AND step.skip_reason IS NOT NULL) AS steps_skipped,
                    (SELECT count(*)::bigint
                       FROM viryaos_play_step_recipients AS recipient
                       JOIN viryaos_play_steps AS step
                         ON step.workspace_id = recipient.workspace_id
                        AND step.id = recipient.step_id
                      WHERE recipient.workspace_id = play.workspace_id
                        AND step.play_id = play.id) AS recipients_reached
                FROM viryaos_plays AS play
                WHERE play.workspace_id = $1
                ORDER BY play.started_at DESC
                LIMIT $2
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(MAX_SNAPSHOTS_PER_CONTEXT)
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
            let standings = self.play_standings_for_read(workspace_id).await?;
            if plays.is_empty() {
                return Ok(PlayLedger {
                    plays: Vec::new(),
                    standings,
                });
            }
            let play_ids: Vec<Uuid> = plays.iter().map(|play| play.play_id).collect();
            let claims = sqlx::query_as::<_, PlayClaimRow>(
                r#"
                SELECT
                    outcome.play_id, outcome.claim,
                    outcome.success_metric_platform, outcome.success_metric_key,
                    outcome.window_start, outcome.window_end, outcome.status,
                    outcome.evidence, outcome.evidence_reason, outcome.effect_assessment,
                    outcome.delta_basis_points, outcome.baseline_milli_per_day,
                    outcome.observed_milli_per_day, outcome.recipients_reached
                FROM viryaos_play_outcomes AS outcome
                WHERE outcome.workspace_id = $1
                  AND outcome.play_id = ANY($2)
                ORDER BY outcome.play_id, outcome.claim
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(&play_ids)
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;

            let mut entries = Vec::with_capacity(plays.len());
            for play in plays {
                let mut views = Vec::new();
                for row in claims.iter().filter(|claim| claim.play_id == play.play_id) {
                    let claim = PlayClaim::parse(&row.claim).ok_or(RepositoryError::Unexpected)?;
                    views.push(PlayClaimView {
                        claim,
                        // Carried on every claim rather than documented once.
                        // The consumer that drops this field is the consumer
                        // that turns a coincidence into a cause.
                        claim_means: claim.description(),
                        success_metric_platform: row.success_metric_platform.clone(),
                        success_metric_key: row.success_metric_key.clone(),
                        window_start: row.window_start,
                        window_end: row.window_end,
                        status: row.status.clone(),
                        evidence: row.evidence.clone(),
                        evidence_reason: row.evidence_reason.clone(),
                        effect: row
                            .effect_assessment
                            .as_deref()
                            .map(parse_effect_assessment)
                            .transpose()?,
                        delta_basis_points: row.delta_basis_points,
                        baseline_milli_per_day: row.baseline_milli_per_day,
                        observed_milli_per_day: row.observed_milli_per_day,
                        recipients_reached: row
                            .recipients_reached
                            .map(u32::try_from)
                            .transpose()
                            .map_err(|_| RepositoryError::Unexpected)?,
                    });
                }
                entries.push(PlayLedgerEntry {
                    play_id: PlayId::from_uuid(play.play_id),
                    kind: PlayKind::parse(&play.play_kind).ok_or(RepositoryError::Unexpected)?,
                    anchor: plays::anchor_ref(
                        PlayAnchorKind::parse(&play.anchor_kind)
                            .ok_or(RepositoryError::Unexpected)?,
                        play.anchor_id,
                    ),
                    anchor_at: play.anchor_at,
                    hypothesis: play.hypothesis,
                    state: play.state,
                    started_at: play.started_at,
                    completed_at: play.completed_at,
                    steps_total: bounded_u32(play.steps_total)?,
                    steps_settled: bounded_u32(play.steps_settled)?,
                    steps_skipped: bounded_u32(play.steps_skipped)?,
                    recipients_reached: bounded_u32(play.recipients_reached)?,
                    claims: views,
                });
            }
            Ok(PlayLedger {
                plays: entries,
                standings,
            })
        })
        .await
    }
}

#[async_trait]
impl AutopilotPlayOutcomeRepository for PostgresAutopilotRepository {
    async fn claim_due_play_outcomes(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
        now: OffsetDateTime,
    ) -> Result<Vec<ClaimedPlayOutcome>, RepositoryError> {
        self.claim_due_play_outcomes_impl(workspace_id, limit, now)
            .await
    }

    async fn observe_play_outcome(
        &self,
        workspace_id: WorkspaceId,
        outcome: &ClaimedPlayOutcome,
        now: OffsetDateTime,
    ) -> Result<PlayOutcomeObservation, RepositoryError> {
        self.observe_play_outcome_impl(workspace_id, outcome, now)
            .await
    }

    async fn complete_play_outcome(
        &self,
        workspace_id: WorkspaceId,
        outcome: &ClaimedPlayOutcome,
        observation: &PlayOutcomeObservation,
        verdict: PlayOutcomeVerdict,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError> {
        self.complete_play_outcome_impl(workspace_id, outcome, observation, verdict, now)
            .await
    }

    async fn fail_play_outcome(
        &self,
        workspace_id: WorkspaceId,
        outcome_id: Uuid,
        error_kind: &'static str,
        retryable: bool,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError> {
        self.fail_play_outcome_impl(workspace_id, outcome_id, error_kind, retryable, now)
            .await
    }
}

/// Folds one play's measured verdict into the record for its kind.
///
/// `None` is an outcome nobody could measure. It is counted so the record is
/// complete and left out of every calculation, because being unable to see
/// whether a play worked is a reason to fix the measurement rather than to stop
/// running the play.
pub(super) async fn record_play_outcome(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    kind: PlayKind,
    assessment: Option<EffectAssessment>,
) -> Result<(), RepositoryError> {
    let existing = sqlx::query_as::<_, PlayLearningRow>(
        r#"
        SELECT play_kind, improved_count, neutral_count, worsened_count,
               insufficient_count, consecutive_worsened, retired_reason
        FROM viryaos_play_learning
        WHERE workspace_id = $1 AND play_kind = $2
        FOR UPDATE
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(kind.as_str())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    let record = existing
        .as_ref()
        .map(play_record)
        .transpose()?
        .unwrap_or_default()
        .observe(assessment);

    // The policy is the operator's, read from the plays context rather than
    // assumed: a workspace that widened the sample size before retiring a play
    // must not have that overruled by a default sitting in this file.
    let policy = play_learning_policy(transaction, workspace_id).await?;
    let standing = assess_play_standing(record, policy);
    let (retired_reason, weight) = match standing {
        PlayStanding::Retired { reason } => (Some(reason.as_str()), 0_i32),
        PlayStanding::Untested { .. } | PlayStanding::Weighted { .. } => {
            (None, i32::from(standing.weight_basis_points()))
        }
    };

    sqlx::query(
        r#"
        INSERT INTO viryaos_play_learning (
            workspace_id, play_kind, improved_count, neutral_count, worsened_count,
            insufficient_count, consecutive_worsened, weight_basis_points,
            retired_at, retired_reason
        )
        VALUES (
            $1,$2,$3,$4,$5,$6,$7,$8,
            CASE WHEN $9::text IS NULL THEN NULL ELSE now() END, $9
        )
        ON CONFLICT (workspace_id, play_kind) DO UPDATE SET
            improved_count = EXCLUDED.improved_count,
            neutral_count = EXCLUDED.neutral_count,
            worsened_count = EXCLUDED.worsened_count,
            insufficient_count = EXCLUDED.insufficient_count,
            consecutive_worsened = EXCLUDED.consecutive_worsened,
            weight_basis_points = EXCLUDED.weight_basis_points,
            -- A retirement keeps the moment it was decided. Refreshing it on
            -- every later write would make an old decision look new.
            retired_at = CASE
                WHEN EXCLUDED.retired_reason IS NULL THEN NULL
                ELSE COALESCE(viryaos_play_learning.retired_at, now())
            END,
            retired_reason = EXCLUDED.retired_reason
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(kind.as_str())
    .bind(i32::try_from(record.improved).unwrap_or(i32::MAX))
    .bind(i32::try_from(record.neutral).unwrap_or(i32::MAX))
    .bind(i32::try_from(record.worsened).unwrap_or(i32::MAX))
    .bind(i32::try_from(record.insufficient).unwrap_or(i32::MAX))
    .bind(i32::try_from(record.consecutive_worsened).unwrap_or(i32::MAX))
    .bind(weight)
    .bind(retired_reason)
    .execute(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct PlayLearningRow {
    play_kind: String,
    improved_count: i32,
    neutral_count: i32,
    worsened_count: i32,
    insufficient_count: i32,
    consecutive_worsened: i32,
    retired_reason: Option<String>,
}

fn play_record(row: &PlayLearningRow) -> Result<PlayRecord, RepositoryError> {
    let count = |value: i32| u32::try_from(value).map_err(|_| RepositoryError::Unexpected);
    Ok(PlayRecord {
        improved: count(row.improved_count)?,
        neutral: count(row.neutral_count)?,
        worsened: count(row.worsened_count)?,
        insufficient: count(row.insufficient_count)?,
        consecutive_worsened: count(row.consecutive_worsened)?,
        // Only an operator sets this, and once set the rule keeps returning it
        // however the counts move.
        operator_retired: row.retired_reason.as_deref()
            == Some(RetirementReason::OperatorRetired.as_str()),
    })
}

async fn play_learning_policy(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
) -> Result<LearningPolicy, RepositoryError> {
    let config = sqlx::query_scalar::<_, Value>(
        "SELECT config FROM viryaos_autopilot_policies WHERE workspace_id=$1 AND context='plays'",
    )
    .bind(workspace_id.into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    Ok(config
        .and_then(|config| serde_json::from_value::<PlayPolicy>(config).ok())
        .unwrap_or_default()
        .learning)
}

impl PostgresAutopilotRepository {
    /// The standings, with the operator's own plays policy behind them.
    ///
    /// The read model resolves the policy itself rather than taking one from
    /// the caller: an HTTP handler holding a stale policy would report a
    /// standing the evaluator does not act on.
    async fn play_standings_for_read(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<PlayKindStanding>, RepositoryError> {
        let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
        let learning = play_learning_policy(&mut transaction, workspace_id).await?;
        transaction.commit().await.map_err(map_sqlx)?;
        self.load_play_standings_impl(
            workspace_id,
            PlayPolicy {
                learning,
                ..PlayPolicy::default()
            },
        )
        .await
    }

    pub(super) async fn load_play_standings_impl(
        &self,
        workspace_id: WorkspaceId,
        policy: PlayPolicy,
    ) -> Result<Vec<PlayKindStanding>, RepositoryError> {
        self.bounded(async {
            let rows = sqlx::query_as::<_, PlayLearningRow>(
                r#"
                SELECT play_kind, improved_count, neutral_count, worsened_count,
                       insufficient_count, consecutive_worsened, retired_reason
                FROM viryaos_play_learning
                WHERE workspace_id = $1
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
            // Every kind is reported, including the ones with no record at all.
            // A kind missing from the read would be a play with no standing,
            // and the caller would have to invent one.
            PlayKind::all()
                .into_iter()
                .map(|kind| {
                    let record = rows
                        .iter()
                        .find(|row| row.play_kind == kind.as_str())
                        .map(play_record)
                        .transpose()?
                        .unwrap_or_default();
                    let standing = assess_play_standing(record, policy.learning);
                    Ok(PlayKindStanding {
                        kind,
                        record,
                        standing,
                        effective_max_recipients_per_step: effective_recipient_ceiling(
                            policy.max_recipients_per_step,
                            standing,
                        ),
                    })
                })
                .collect()
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Outreach kind learning — same discipline, keyed on OutreachTargetKind.
// ---------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct OutreachKindLearningRow {
    target_kind: String,
    improved_count: i32,
    neutral_count: i32,
    worsened_count: i32,
    insufficient_count: i32,
    consecutive_worsened: i32,
    retired_reason: Option<String>,
}

fn outreach_kind_record(row: &OutreachKindLearningRow) -> Result<PlayRecord, RepositoryError> {
    let count = |value: i32| u32::try_from(value).map_err(|_| RepositoryError::Unexpected);
    Ok(PlayRecord {
        improved: count(row.improved_count)?,
        neutral: count(row.neutral_count)?,
        worsened: count(row.worsened_count)?,
        insufficient: count(row.insufficient_count)?,
        consecutive_worsened: count(row.consecutive_worsened)?,
        operator_retired: row.retired_reason.as_deref()
            == Some(RetirementReason::OperatorRetired.as_str()),
    })
}

impl PostgresAutopilotRepository {
    /// All outreach kind standings, with every kind reported even if it has no
    /// record yet — a kind missing from the read would be a wave with no
    /// standing, and the caller would have to invent one.
    pub(super) async fn load_outreach_kind_standings_impl(
        &self,
        workspace_id: WorkspaceId,
        max_pitches_per_wave: u32,
    ) -> Result<Vec<OutreachKindStanding>, RepositoryError> {
        self.bounded(async {
            let rows = sqlx::query_as::<_, OutreachKindLearningRow>(
                r#"
                SELECT target_kind, improved_count, neutral_count, worsened_count,
                       insufficient_count, consecutive_worsened, retired_reason
                FROM viryaos_outreach_kind_learning
                WHERE workspace_id = $1
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
            OutreachTargetKind::all()
                .into_iter()
                .map(|kind| {
                    let record = rows
                        .iter()
                        .find(|row| row.target_kind == kind.as_str())
                        .map(outreach_kind_record)
                        .transpose()?
                        .unwrap_or_default();
                    let standing = assess_play_standing(record, LearningPolicy::default());
                    Ok(OutreachKindStanding {
                        kind,
                        record,
                        standing,
                        effective_max_pitches_per_wave: effective_wave_ceiling(
                            max_pitches_per_wave,
                            standing,
                        ),
                    })
                })
                .collect()
        })
        .await
    }
}
