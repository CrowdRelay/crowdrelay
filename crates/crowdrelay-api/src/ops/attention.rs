#[derive(Debug, Serialize)]
struct AttentionEcosystemOverview {
    schema_version: u32,
    flags: Vec<crate::ecosystem::FeatureFlag>,
    last_reconciliation: Option<crate::ecosystem::ReconciliationRun>,
    open_findings: i64,
    next_event: Option<crate::ecosystem::OverviewEvent>,
    bandsintown_sync: Option<crate::ecosystem::BandsintownSyncStatus>,
}

/// Lightweight summary of a pending autopilot action — just the fields the
/// AttentionInbox needs to render an approval item. NOT the full
/// `PendingAutopilotAction` (which includes payload, briefing, assignee,
/// executor readiness, etc.) — the attention snapshot is a summary view,
/// not a detail modal.
#[derive(Debug, Serialize, sqlx::FromRow)]
struct PendingActionSummary {
    id: uuid::Uuid,
    context: String,
    action_kind: String,
    subject_kind: String,
    /// The community the action targets, when the payload names one.
    subreddit: Option<String>,
    /// What the content is called, when the payload names one.
    title: Option<String>,
    /// Which workflow template the action runs, when the payload names one.
    template_id: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    approval_expires_at: Option<OffsetDateTime>,
    /// Total matching rows across the whole query, not just this page.
    /// Carried on every row by the window function; not part of the wire
    /// shape.
    #[serde(skip)]
    total_count: i64,
}

/// One channel's backlog of drafted-but-unpublished posts.
///
/// Reported per channel because the answer differs by channel: Reddit needs a
/// human by policy, while Telegram and Discord are one environment variable
/// away from publishing themselves.
#[derive(Debug, Serialize, sqlx::FromRow)]
struct UnpublishedDraftChannel {
    /// `reddit`, `telegram`, `discord` or `social`.
    channel: String,
    drafts: i64,
    /// When the oldest draft on this channel was created. The age is the
    /// point: one draft from this morning is a queue, twelve from last month
    /// is a channel nobody is running.
    #[serde(with = "time::serde::rfc3339::option")]
    oldest_drafted_at: Option<OffsetDateTime>,
}

/// One community the brain wants to post to and cannot, because nobody has
/// joined it.
///
/// Joining is a prerequisite of posting: many subreddits refuse a post from a
/// non-member outright, and posting to a community you have not joined is the
/// pattern that gets an account flagged -- the account the whole discovery
/// loop reads through. So the gate is correct, and what it costs has to be
/// visible or it reads as a brain with nothing to say.
#[derive(Debug, Serialize, sqlx::FromRow)]
struct BlockedCommunity {
    /// The community, as the operator would search for it.
    community: String,
    /// Members, when discovery recorded it. The operator's own tiebreak
    /// between two communities the brain wants equally.
    member_count: Option<i32>,
    /// When this community was discovered. A backlog that is weeks old is a
    /// channel nobody is running, which is a different problem from one that
    /// filled up this morning.
    #[serde(with = "time::serde::rfc3339::option")]
    discovered_at: Option<OffsetDateTime>,
}

#[derive(Debug, Serialize)]
struct OperatorAttentionSnapshot {
    summary: OpsSummary,
    alerts: Vec<OpsAlert>,
    dead_outbox: Vec<OutboxItem>,
    dead_deliveries: Vec<DeliveryItem>,
    dead_push: Vec<PushDeliveryItem>,
    ecosystem: AttentionEcosystemOverview,
    findings: Vec<crate::ecosystem::ReconciliationFinding>,
    /// Pending autopilot actions awaiting human approval. Comes from the
    /// same authoritative query as `load_control_overview` — just the
    /// summary fields the inbox renders, not the full action detail.
    needs_you: Vec<PendingActionSummary>,
    /// Count of opportunities awaiting approval. Derived from authoritative
    /// action state, not from rendered UI items.
    awaiting_approval: i64,
    /// Dispatches the brain produced that are still waiting for a person to
    /// publish them.
    ///
    /// This belongs in an exception-first view because it is the one queue
    /// where the system is blocked on the operator rather than the other way
    /// round. Every outbound channel drafts and waits: Reddit is read-only by
    /// policy, Telegram, Discord and social default to manual. A draft nobody
    /// publishes reaches nobody, so the work the brain did is spent and the
    /// fan it would have brought does not arrive.
    unpublished_drafts: Vec<UnpublishedDraftChannel>,
    /// Communities the brain has decided it wants to post to and cannot,
    /// because nobody has joined them.
    ///
    /// The sibling of `unpublished_drafts` and the earlier half of the same
    /// story. A draft is work the brain finished and nobody published; this is
    /// work the brain cannot even start. Production reached 119 discovered
    /// communities with none joined, which gated out every community candidate
    /// -- no decision row, no draft, nothing in any queue. The Reddit channel
    /// read as idle when it was blocked on one manual step.
    ///
    /// Most-recently-discovered first, capped: this is a prompt to go and join
    /// something, not a directory.
    blocked_communities: Vec<BlockedCommunity>,
    /// What the brain makes of its own recent performance.
    ///
    /// This view exists to answer "what needs me?", and a fanbase that is
    /// shrinking, or flat for a month, is the largest thing that can need a
    /// person -- larger than any dead outbox row. It was computed on
    /// `/v1/admin/ops/cycles` and nowhere else, so an operator only saw it if
    /// they went looking at a cycle list, which is the opposite of
    /// exception-first.
    brain: BrainSelfAssessment,
    /// What the approval queue lost, and what it is about to lose.
    ///
    /// The queue's other half. `needs_you` above is what is pending; this is
    /// the asks that reached their deadline — reaped by the sweep, or past it
    /// and waiting for the sweep to notice. Without it an approval nobody
    /// answered simply disappears, and with every outbound channel in this
    /// system drafting and waiting for a person, that silence is the most
    /// expensive one there is: nothing anywhere records that the thing was
    /// proposed at all.
    lapsed_approvals: LapsedApprovals,
    /// Outward sends that failed in the last week, named (O.7).
    ///
    /// The overview counts them. A count says something broke; it does not say
    /// which promoter never heard from the band, which is the only version of
    /// that fact an operator can act on.
    failed_sends: FailedSends,
    /// Escalations the brain raised for the band, newest per subject.
    ///
    /// Every `show.escalation` emission is a durable outbox row — the emit is
    /// the record that a task needed a person or a report came due, and an
    /// email that never left does not un-happen it. Until now that record was
    /// only visible to the outbox machinery: a parked `show.escalation` lane
    /// meant the band never heard about the thing the brain flagged, and the
    /// attention board — the view that exists to answer "what needs me?" —
    /// said nothing. This reads the source directly, deduplicated per
    /// subject so a re-raised task shows once at its latest raise.
    band_notices: Vec<BandNotice>,
}

/// The brain's own verdict, and whether it is asking for a person.
#[derive(Debug, Serialize)]
struct BrainSelfAssessment {
    /// `improving`, `learning`, `stagnant`, `regressing`, or `initializing`.
    state: &'static str,
    /// True only for `regressing` and `stagnant`. A flat North Star on a young
    /// system is expected, and raising it as an exception every five minutes is
    /// how an operator learns to stop reading the exceptions.
    needs_attention: bool,
    /// Distinct days of North Star readings behind the verdict. `initializing`
    /// with a small number here is a system that has not watched itself for
    /// long enough, which is a different thing from one that cannot decide.
    days_observed: usize,
    /// Consecutive finished cycles that produced no actions, counting back
    /// from the latest. A brain that has been quiet for three days straight
    /// is a different thing from one quiet since the last check — the count
    /// is what makes the silence legible.
    quiet_cycles: i64,
    /// Why the most recent quiet cycle stayed quiet, in the brain's own words
    /// ("WAIT wins: VOI=0.85 > best_action_value=0.00"). The first principle
    /// of the plan is that the system may do nothing — and says so. NULL when
    /// no quiet cycle has a recorded reason — the cycle is acting, or it ran
    /// before migration 0268.
    latest_wait_reason: Option<String>,
    /// The brain's report on its own prediction accuracy, per estimation
    /// regime, from the persisted causal-model checkpoint. `null` when no
    /// checkpoint exists yet — a missing answer is null, never zero. Each
    /// regime is independently nullable: an empty treatment-effect regime is
    /// itself the honest answer ("no paired experiment evidence yet").
    calibration: Option<CalibrationReadout>,
}

/// One regime's calibration report, reduced to what an operator acts on.
#[derive(Debug, Serialize)]
pub(crate) struct CalibrationRegimeReadout {
    /// Prediction-observation pairs behind the numbers — the count is the
    /// honesty: a slope computed from 3 pairs reads differently than one
    /// from 300.
    predictions: usize,
    /// Mean (predicted - observed). Positive means the brain over-promises.
    bias: f64,
    /// Regression slope of observed on predicted. 1.0 is calibrated; below
    /// one is over-confident, above is under-confident.
    slope: f64,
    /// Mean absolute error in fan units.
    mae: f64,
}

/// The three estimation regimes' calibration, each independently nullable.
#[derive(Debug, Serialize)]
pub(crate) struct CalibrationReadout {
    /// Directly observed durable fans — highest trust, and the thinnest data.
    y30_direct: Option<CalibrationRegimeReadout>,
    /// Y14 bridged to Y30 — medium trust.
    y14_bridged: Option<CalibrationRegimeReadout>,
    /// The observational outcome model — what EFE scores on today.
    outcome_model: Option<CalibrationRegimeReadout>,
}

pub async fn attention(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let timeout_duration = state.ops.operation_timeout;
    // Fourteen reads, each paying one permit of the process-wide control-plane
    // budget — the ecosystem arm pays its leaves individually inside
    // `load_attention_ecosystem`. Without a shared bound this page asks for
    // more connections than the pool has and holds every one of them, so any
    // other request to this API waits behind a single operator refresh; the
    // budget turns that into queueing — see `ops::ControlPlaneReadBudget`.
    let budget = &state.read_budget;
    let summary = run_limited(budget, timeout_duration, load_summary(&state.ops));
    let alerts = run_limited(budget, timeout_duration, load_alerts(&state.ops));
    let dead_outbox = run_limited(budget, timeout_duration, load_dead_outbox(&state.ops));
    let dead_deliveries =
        run_limited(budget, timeout_duration, load_dead_deliveries(&state.ops));
    let dead_push = run_limited(budget, timeout_duration, load_dead_push(&state.ops));
    let ecosystem = run_with_timeout(timeout_duration, load_attention_ecosystem(&state));
    let findings = run_limited(budget, timeout_duration, load_open_findings(&state));
    let needs_you = run_limited(budget, timeout_duration, load_needs_you(&state.ops));
    let brain = run_limited(budget, timeout_duration, load_brain_assessment(&state.ops));
    let unpublished_drafts =
        run_limited(budget, timeout_duration, load_unpublished_drafts(&state));
    let blocked_communities =
        run_limited(budget, timeout_duration, load_blocked_communities(&state.ops));
    let lapsed = run_limited(budget, timeout_duration, load_lapsed_approvals(&state.ops));
    let failed = run_limited(budget, timeout_duration, load_failed_sends(&state.ops));
    let notices = run_limited(budget, timeout_duration, load_band_notices(&state.ops));

    let (
        summary, alerts, dead_outbox, dead_deliveries, dead_push,
        ecosystem, findings, needs_you, brain, unpublished_drafts,
        blocked_communities, lapsed, failed, notices,
    ) = tokio::join!(
        summary, alerts, dead_outbox, dead_deliveries, dead_push,
        ecosystem, findings, needs_you, brain, unpublished_drafts,
        blocked_communities, lapsed, failed, notices,
    );

    let request_id_value = request_id(&headers);
    let summary = match summary {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id_value),
    };
    let alerts = match alerts {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    let dead_outbox = match dead_outbox {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    let dead_deliveries = match dead_deliveries {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    let dead_push = match dead_push {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    let ecosystem = match ecosystem {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    let findings = match findings {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    let needs_you = match needs_you {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    let brain = match brain {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    let unpublished_drafts = match unpublished_drafts {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    let blocked_communities = match blocked_communities {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    let lapsed_approvals = match lapsed {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    let failed_sends = match failed {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    let band_notices = match notices {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id(&headers)),
    };

    let (needs_you, awaiting_approval) = needs_you;

    private_json(
        StatusCode::OK,
        OperatorAttentionSnapshot {
            summary,
            alerts,
            dead_outbox,
            dead_deliveries,
            dead_push,
            ecosystem,
            findings,
            needs_you,
            awaiting_approval,
            unpublished_drafts,
            blocked_communities,
            brain,
            lapsed_approvals,
            failed_sends,
            band_notices,
        },
    )
}

/// The drafted posts waiting on a person, per channel.
///
/// Counts the draft states rather than excluding the published one, so a
/// status added later is not silently reported as a backlog. `rate_limited`
/// and `failed` are deliberately absent: those are the system's problem and
/// already surface as alerts, while `awaiting_manual_post` is the operator's.
async fn load_unpublished_drafts(
    state: &crate::AppState,
) -> Result<Vec<UnpublishedDraftChannel>, OpsError> {
    sqlx::query_as::<_, UnpublishedDraftChannel>(
        r#"
        SELECT channel, count(*)::bigint AS drafts, min(created_at) AS oldest_drafted_at
        FROM (
            SELECT 'reddit' AS channel, created_at FROM community_posts
            WHERE workspace_id = $1 AND status = 'awaiting_manual_post'
            UNION ALL
            SELECT 'telegram', created_at FROM telegram_posts
            WHERE workspace_id = $1 AND status = 'awaiting_manual_post'
            UNION ALL
            SELECT 'discord', created_at FROM discord_posts
            WHERE workspace_id = $1 AND status = 'awaiting_manual_post'
            UNION ALL
            SELECT 'social', created_at FROM social_posts
            WHERE workspace_id = $1 AND status = 'awaiting_manual_post'
        ) AS drafts
        GROUP BY channel
        ORDER BY min(created_at)
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .fetch_all(state.ticketing.pool())
    .await
    .map_err(OpsError::sqlx)
}

/// Communities with a wanted post and no membership.
///
/// The predicate is the join worker's own definition of demand -- an active,
/// unjoined subreddit carrying a promoted, unrefused community target -- so
/// this surface, the `crowdrelay_brain_communities_blocked_on_join` gauge and
/// the worker's own ordering all agree on what "wanted" means instead of
/// drifting into three different answers.
async fn load_blocked_communities(state: &OpsState) -> Result<Vec<BlockedCommunity>, OpsError> {
    sqlx::query_as::<_, BlockedCommunity>(
        r#"
        SELECT place.name AS community,
               place.member_count,
               place.created_at AS discovered_at
        FROM discovery_places AS place
        WHERE place.workspace_id = $1
          AND place.place_kind = 'subreddit'
          AND place.membership_state = 'not_joined'
          AND place.status = 'active'
          AND EXISTS (
                SELECT 1 FROM agent_outreach_targets AS t
                 WHERE t.workspace_id = place.workspace_id
                   AND t.place_id = place.id
                   AND t.status = 'promoted'
                   AND t.target_kind = 'community'
                   AND t.subreddit IS NOT NULL
                   AND t.screening_verdict IS DISTINCT FROM 'refused'
              )
        ORDER BY place.member_count DESC NULLS LAST, place.name
        LIMIT 20
        "#,
    )
    .bind(state.workspace_id().into_uuid())
    .fetch_all(&state.pool)
    .await
    .map_err(OpsError::sqlx)
}

/// The brain's verdict on itself, from the same daily series `/ops/cycles`
/// reports. One query, one interpretation, so the two surfaces cannot disagree
/// about whether the fanbase is growing.
async fn load_brain_assessment(state: &OpsState) -> Result<BrainSelfAssessment, OpsError> {
    let samples = load_north_star_days(state).await?;
    let days_observed = samples.len();
    let assessment = assess(samples);
    // The quiet streak: consecutive finished cycles that created no actions,
    // counted back from the latest, plus the brain's own reason for the most
    // recent one. The system may do nothing and say so — this is where the
    // saying surfaces, because this is the page an operator opens to ask
    // "what needs me?" and a silent brain is a thing that can need them.
    let quiet = sqlx::query_as::<_, (i64, Option<String>)>(
        r#"
        SELECT
            (SELECT count(*) FROM viryaos_autopilot_cycle_runs
             WHERE workspace_id = $1 AND finished_at IS NOT NULL
               AND actions_created = 0
               AND started_at > COALESCE((
                   SELECT max(started_at) FROM viryaos_autopilot_cycle_runs
                   WHERE workspace_id = $1 AND finished_at IS NOT NULL
                     AND actions_created > 0
               ), '-infinity'::timestamptz)) AS quiet_cycles,
            (SELECT wait_reason FROM viryaos_autopilot_cycle_runs
             WHERE workspace_id = $1 AND finished_at IS NOT NULL
               AND actions_created = 0
               AND wait_reason IS NOT NULL
             ORDER BY started_at DESC LIMIT 1) AS latest_wait_reason
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .fetch_one(&state.pool)
    .await
    .map_err(OpsError::sqlx)?;
    let calibration = load_calibration_readout(state).await;
    Ok(BrainSelfAssessment {
        state: assessment.as_str(),
        needs_attention: assessment.needs_attention(),
        days_observed,
        quiet_cycles: quiet.0,
        latest_wait_reason: quiet.1,
        calibration,
    })
}

/// The brain's own prediction-vs-outcome record, read out of the causal-model
/// checkpoint. `record_by_regime` has written this since the first evidence
/// replay; what was missing is anyone reading it back. A checkpoint that is
/// absent, shaped differently, or unreadable degrades to `None` — the
/// assessment must not fail because a diagnostic could not be read.
async fn load_calibration_readout(state: &OpsState) -> Option<CalibrationReadout> {
    let state_json = sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT state FROM viryaos_brain_state WHERE workspace_id = $1 AND module = 'causal_model'",
    )
    .bind(state.workspace_id().into_uuid())
    .fetch_optional(&state.pool)
    .await
    .ok()??;
    let readout = calibration_readout_from(&state_json);
    if readout.is_none() {
        tracing::warn!(
            workspace_id = %state.workspace_id().into_uuid(),
            "causal-model checkpoint has no readable calibration subtree"
        );
    }
    readout
}

/// The checkpoint's `calibration` subtree -> per-regime readouts. `None` when
/// the key is absent (a checkpoint from before the regime split) or does not
/// deserialize — the report degrades rather than the page.
fn calibration_readout_from(state_json: &serde_json::Value) -> Option<CalibrationReadout> {
    let regimes = serde_json::from_value::<crowdrelay_application::CalibrationByRegime>(
        state_json.get("calibration")?.clone(),
    )
    .ok()?;
    let readout = |tracker: &crowdrelay_application::CalibrationTracker| {
        let report: crowdrelay_application::CalibrationReport = tracker.report();
        (report.n > 0).then_some(CalibrationRegimeReadout {
            predictions: report.n,
            bias: report.bias,
            slope: report.calibration_slope,
            mae: report.mae,
        })
    };
    Some(CalibrationReadout {
        y30_direct: readout(&regimes.y30_direct),
        y14_bridged: readout(&regimes.y14_bridged),
        outcome_model: readout(&regimes.outcome_model),
    })
}

/// Open watchdog alerts, plus the ones that recovered in the last 24 hours.
///
/// Recovered rows stay visible for a day because the watchdog only re-evaluates
/// every five minutes: an operator who fixed the cause needs to see that the
/// alert closed by itself rather than wonder whether the count is stuck.
async fn load_alerts(state: &OpsState) -> Result<Vec<OpsAlert>, OpsError> {
    sqlx::query_as::<_, OpsAlert>(
        r#"
        SELECT alert_key, severity, summary, active, first_seen_at, last_seen_at,
               last_alerted_at, recovered_at, details
        FROM viryaos_ops_alert_state
        WHERE workspace_id = $1
          AND (active OR recovered_at >= now() - INTERVAL '24 hours')
        ORDER BY active DESC,
                 (severity = 'critical') DESC,
                 last_seen_at DESC,
                 alert_key
        LIMIT 50
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .fetch_all(&state.pool)
    .await
    .map_err(OpsError::sqlx)
}

async fn load_dead_outbox(state: &OpsState) -> Result<Vec<OutboxItem>, OpsError> {
    sqlx::query_as::<_, OutboxItem>(
        r#"
        SELECT id, event_type, event_version, status, attempts, max_attempts,
               available_at, last_error_kind, created_at, updated_at,
               delivered_at, dead_at
        FROM outbox_events
        WHERE workspace_id = $1 AND status = 'dead'
        ORDER BY created_at DESC, id DESC
        LIMIT 50
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .fetch_all(&state.pool)
    .await
    .map_err(OpsError::sqlx)
}

async fn load_dead_deliveries(state: &OpsState) -> Result<Vec<DeliveryItem>, OpsError> {
    sqlx::query_as::<_, DeliveryItem>(
        r#"
        SELECT delivery.id, delivery.outbox_event_id, event.event_type,
               endpoint.name AS endpoint_name, endpoint.active AS endpoint_active,
               delivery.status, delivery.attempt_count, delivery.max_attempts,
               delivery.available_at, delivery.last_response_status,
               delivery.last_error_kind, delivery.created_at, delivery.updated_at,
               delivery.delivered_at, delivery.dead_at
        FROM webhook_deliveries AS delivery
        JOIN outbox_events AS event
          ON event.workspace_id = delivery.workspace_id
         AND event.id = delivery.outbox_event_id
        JOIN webhook_endpoints AS endpoint
          ON endpoint.workspace_id = delivery.workspace_id
         AND endpoint.id = delivery.endpoint_id
        WHERE delivery.workspace_id = $1 AND delivery.status = 'dead'
        ORDER BY delivery.created_at DESC, delivery.id DESC
        LIMIT 50
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .fetch_all(&state.pool)
    .await
    .map_err(OpsError::sqlx)
}

async fn load_dead_push(state: &OpsState) -> Result<Vec<PushDeliveryItem>, OpsError> {
    sqlx::query_as::<_, PushDeliveryItem>(
        r#"
        SELECT id, fan_id, source_kind, title, status, attempt_count,
               error_code, available_at, created_at, delivered_at, completed_at
        FROM fan_push_deliveries
        WHERE workspace_id = $1 AND status IN ('failed', 'ambiguous')
          AND error_code IS DISTINCT FROM 'preference_disabled'
        ORDER BY created_at DESC, id DESC
        LIMIT 50
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .fetch_all(&state.pool)
    .await
    .map_err(OpsError::sqlx)
}

async fn load_band_notices(state: &OpsState) -> Result<Vec<BandNotice>, OpsError> {
    // The `show.escalation` event set — every kind the capability map routes
    // to the band-facing lane — kept as a literal list because this surface
    // reads the durable record, not the map that emitted it. A kind added to
    // the lane without an entry here is an escalation that happened and no
    // screen shows; the contract test pins the two together.
    sqlx::query_as::<_, BandNotice>(
        r#"
        SELECT deduped.id, deduped.kind, deduped.detail, deduped.delivered,
               deduped.created_at
        FROM (
            SELECT DISTINCT ON (scoped.event_type, scoped.subject_key)
                   scoped.id,
                   substring(scoped.event_type from 'crowdrelay\.(.*)') AS kind,
                   scoped.payload AS detail,
                   (scoped.status = 'delivered') AS delivered,
                   scoped.created_at
            FROM (
                SELECT id, event_type, payload, status, created_at,
                       COALESCE(
                           NULLIF(payload->>'event_id', ''),
                           NULLIF(payload->>'release_id', ''),
                           NULLIF(payload->>'opportunity_id', ''),
                           id::text
                       ) || ':' || COALESCE(payload->>'task', '') AS subject_key
                FROM outbox_events
                WHERE workspace_id = $1
                  AND event_type IN (
                      'crowdrelay.show.task_attention_required',
                      'crowdrelay.show.post_show_report_due',
                      'crowdrelay.release.r3_report_due',
                      'crowdrelay.release.r14_report_due',
                      'crowdrelay.release.likely_listeners',
                      'crowdrelay.release.editorial_pitch_parked',
                      'crowdrelay.release.editorial_pitch_escalated',
                      'crowdrelay.opportunity.counterparty_report_issued'
                  )
                  AND created_at > now() - INTERVAL '14 days'
            ) AS scoped
            ORDER BY scoped.event_type, scoped.subject_key, scoped.created_at DESC
        ) AS deduped
        ORDER BY deduped.created_at DESC
        LIMIT 50
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .fetch_all(&state.pool)
    .await
    .map_err(OpsError::sqlx)
}

async fn load_attention_ecosystem(
    state: &crate::AppState,
) -> Result<AttentionEcosystemOverview, OpsError> {
    // Seed the lazy defaults exactly as `/ecosystem/overview` does. Without
    // this a workspace whose flags have never been written reports an empty
    // flag list here while the dedicated endpoint reports the full default
    // set, so the two views of the same tenant disagree.
    let budget = &state.read_budget;
    hold(budget, crate::ecosystem::ensure_default_flags(state))
        .await
        .map_err(|_| OpsError::Unexpected)?;
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let flags = sqlx::query_as::<_, crate::ecosystem::FeatureFlag>(
        r#"
        SELECT key, enabled, reason, version, updated_at
        FROM ecosystem_feature_flags
        WHERE workspace_id = $1
        ORDER BY key
        "#,
    )
    .bind(workspace_id);
    let flags = hold(budget, flags.fetch_all(state.ticketing.pool()));
    let last_reconciliation = sqlx::query_as::<_, crate::ecosystem::ReconciliationRun>(
        r#"
        SELECT id, status, trigger, finding_count, started_at, finished_at
        FROM reconciliation_runs
        WHERE workspace_id = $1
        ORDER BY started_at DESC, id DESC
        LIMIT 1
        "#,
    )
    .bind(workspace_id);
    let last_reconciliation =
        hold(budget, last_reconciliation.fetch_optional(state.ticketing.pool()));
    let open_findings = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM reconciliation_findings WHERE workspace_id = $1 AND resolved_at IS NULL",
    )
    .bind(workspace_id);
    let open_findings = hold(budget, open_findings.fetch_one(state.ticketing.pool()));
    let next_event = sqlx::query_as::<_, crate::ecosystem::OverviewEvent>(
        r#"
        SELECT id, slug, title, venue, starts_at
        FROM events
        WHERE workspace_id = $1 AND status = 'published'
          AND starts_at >= now() - interval '6 hours'
        ORDER BY starts_at, id
        LIMIT 1
        "#,
    )
    .bind(workspace_id);
    let next_event = hold(budget, next_event.fetch_optional(state.ticketing.pool()));
    let bandsintown_sync = sqlx::query_as::<_, crate::ecosystem::BandsintownSyncStatus>(
        r#"
        SELECT last_synced_at, last_success_at, next_sync_at, consecutive_failures, last_error,
               (sync_lease_until IS NOT NULL AND sync_lease_until > now()) AS in_progress
        FROM event_sources
        WHERE workspace_id = $1 AND provider = 'bandsintown' AND active
        ORDER BY id
        LIMIT 1
        "#,
    )
    .bind(workspace_id);
    let bandsintown_sync =
        hold(budget, bandsintown_sync.fetch_optional(state.ticketing.pool()));

    let (flags, last_reconciliation, open_findings, next_event, bandsintown_sync) =
        tokio::try_join!(flags, last_reconciliation, open_findings, next_event, bandsintown_sync)
            .map_err(OpsError::sqlx)?;

    Ok(AttentionEcosystemOverview {
        schema_version: crate::ecosystem::SHOW_SNAPSHOT_SCHEMA,
        flags,
        last_reconciliation,
        open_findings,
        next_event,
        bandsintown_sync,
    })
}

async fn load_open_findings(
    state: &crate::AppState,
) -> Result<Vec<crate::ecosystem::ReconciliationFinding>, OpsError> {
    sqlx::query_as::<_, crate::ecosystem::ReconciliationFinding>(
        r#"
        SELECT id, run_id, kind, severity, entity_type, entity_id,
               entity_label, summary, suggested_action, metadata,
               created_at, resolved_at
        FROM reconciliation_findings
        WHERE workspace_id = $1 AND resolved_at IS NULL
        ORDER BY created_at DESC, id DESC
        LIMIT 50
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .fetch_all(state.ticketing.pool())
    .await
    .map_err(OpsError::sqlx)
}

/// Load pending autopilot actions awaiting human approval — the same query
/// as `load_control_overview` branch C, but only the summary fields the
/// AttentionInbox renders. NOT the full `PendingAutopilotAction` with
/// payload, briefing, assignee, and executor readiness.
///
/// Returns the page of actions AND the total count in a single query,
/// instead of two separate scans of the same WHERE clause.
async fn load_needs_you(
    state: &OpsState,
) -> Result<(Vec<PendingActionSummary>, i64), OpsError> {
    let rows = sqlx::query_as::<_, PendingActionSummary>(
        r#"
        SELECT
            id,
            context,
            action_kind,
            subject_kind,
            payload ->> 'subreddit' AS subreddit,
            payload ->> 'title' AS title,
            payload ->> 'template_id' AS template_id,
            approval_expires_at,
            count(*) OVER ()::bigint AS total_count
        FROM viryaos_autopilot_actions
        WHERE workspace_id = $1
          AND status = 'awaiting_approval'
          AND (approval_expires_at IS NULL OR approval_expires_at > now())
          -- A delivery inside a community relay batch asks through the
          -- batch card, not this list — one card for the spread, not one
          -- per community it lands in.
          AND NOT (
              action_kind = 'community.engage.request'
              AND payload ->> 'source_id' IS NOT NULL
          )
        ORDER BY created_at, id
        LIMIT 50
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .fetch_all(&state.pool)
    .await
    .map_err(OpsError::sqlx)?;
    let total = rows.first().map_or(0, |r| r.total_count);
    Ok((rows, total))
}

/// What the approval queue already lost, and what it loses next.
///
/// Delegates to `crowdrelay-infra` rather than carrying the SQL here: the
/// three ways an ask can die, and the rule that they stay apart, are a
/// property of the action ledger and not of this page.
async fn load_lapsed_approvals(state: &OpsState) -> Result<LapsedApprovals, OpsError> {
    crowdrelay_infra::lapsed_approvals::lapsed_approvals(
        &state.pool,
        state.workspace_id.into_uuid(),
        OffsetDateTime::now_utc(),
    )
    .await
    .map_err(OpsError::sqlx)
}

/// Outward sends that failed inside the window, with the recipients who never
/// heard anything.
///
/// Delegates to `crowdrelay-infra` for the same reason `load_lapsed_approvals`
/// does: which failures count as an outward send is a property of the action
/// ledger's `action_class`, not of this page.
async fn load_failed_sends(state: &OpsState) -> Result<FailedSends, OpsError> {
    crowdrelay_infra::sent_record::failed_sends(
        &state.pool,
        state.workspace_id.into_uuid(),
        OffsetDateTime::now_utc(),
    )
    .await
    .map_err(OpsError::sqlx)
}

#[cfg(test)]
mod calibration_readout_tests {
    use super::calibration_readout_from;
    use crowdrelay_application::CalibrationByRegime;

    /// No `calibration` key — a checkpoint serialized before the regime
    /// split — reads as "not reported", not zeroed numbers.
    #[test]
    fn absent_calibration_key_is_none() {
        assert!(calibration_readout_from(&serde_json::json!({})).is_none());
        assert!(
            calibration_readout_from(&serde_json::json!({"calibration": "junk"})).is_none()
        );
    }

    /// A tracked-but-unobserved regime reports `null`, so the operator sees
    /// "no predictions scored yet", never a fabricated zero bias.
    #[test]
    fn unobserved_regimes_serialize_null_not_zero() {
        let regimes = CalibrationByRegime::new();
        let state = serde_json::json!({"calibration": regimes});
        let readout = calibration_readout_from(&state).expect("readout");
        let json = serde_json::to_value(&readout).unwrap();
        assert!(json["y30_direct"].is_null());
        assert!(json["y14_bridged"].is_null());
        assert!(json["outcome_model"].is_null());
    }

    /// A populated regime reports the real numbers; empty siblings stay null.
    #[test]
    fn populated_regime_reports_counts_and_bias() {
        let mut regimes = CalibrationByRegime::new();
        regimes.outcome_model.record("t", 10.0, 1.0, 4.0);
        regimes.outcome_model.record("t", 8.0, 1.0, 6.0);
        let state = serde_json::json!({"calibration": regimes});
        let readout = calibration_readout_from(&state).expect("readout");
        let json = serde_json::to_value(&readout).unwrap();
        assert_eq!(json["outcome_model"]["predictions"], 2);
        // (10-4 + 8-6) / 2 = 4.0 — systematic over-prediction.
        assert_eq!(json["outcome_model"]["bias"], 4.0);
        assert!(json["y30_direct"].is_null());
    }
}
