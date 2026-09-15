// The gig page's one night: the T-21→T+7 ladder as nine steps, each backed
// by the artifact that records it. `control_plane_events` answers "which
// shows"; this answers "what state is Friday in" — each step's state, the
// owner the handoff index names, and the one action open now, in time order.
//
// Every query here reads an existing workspace-scoped artifact table;
// nothing in this file writes, and nothing in the response carries a
// campaign credential or a per-fan row. Missing evidence is null, not
// zero — a step whose measurement does not exist yet says so by carrying
// no detail rather than a fabricated count.

#[derive(Debug, FromRow)]
struct TimelineEventRow {
    id: Uuid,
    slug: String,
    title: String,
    venue: Option<String>,
    status: String,
    starts_at: OffsetDateTime,
    ends_at: Option<OffsetDateTime>,
}

#[derive(Debug, FromRow)]
struct LifecycleEmissionRow {
    phase: String,
    emitted_at: OffsetDateTime,
}

#[derive(Debug, FromRow)]
struct TimelineSurfaceRow {
    surface_key: String,
    status: String,
}

#[derive(Debug, FromRow)]
struct ShowGrowthActionRow {
    id: Uuid,
    status: String,
    lever: Option<String>,
    available_at: OffsetDateTime,
    finished_at: Option<OffsetDateTime>,
}

#[derive(Debug, FromRow)]
struct TimelineAssignmentRow {
    source_kind: String,
    source_ref: Option<String>,
    action_id: Option<Uuid>,
    status: String,
    due_at: Option<OffsetDateTime>,
    // display_name is nullable on workspace_members — a NULL must not decode
    // into a 503 for the whole page. The owner slot stays empty for an
    // unnamed assignee rather than leaking the member's sign-in address.
    display_name: Option<String>,
}

#[derive(Debug, FromRow)]
struct TimelinePlayStepRow {
    step_kind: String,
    due_at: OffsetDateTime,
    settled_at: Option<OffsetDateTime>,
    skip_reason: Option<String>,
}

#[derive(Debug, FromRow)]
struct TimelineChecklistRow {
    item_key: String,
    status: String,
}

#[derive(Debug, FromRow)]
struct TimelineCountsRow {
    nearby_notified: i64,
    qr_campaigns: i64,
    checkins: i64,
}

#[derive(Debug, FromRow)]
struct TimelinePaceRow {
    capacity: Option<i64>,
    paid_tickets: i64,
    paid_tickets_last_7d: i64,
}

#[derive(Debug, FromRow)]
struct TimelineDecisionRow {
    reason: String,
    evaluated_at: OffsetDateTime,
}

#[derive(Debug, FromRow)]
struct TimelineHarvestRow {
    occurred_at: Option<OffsetDateTime>,
    pending_requests: i64,
    succeeded_requests: i64,
}

#[derive(Debug, FromRow)]
struct TimelineCostRow {
    predicted_total_cost_minor: Option<i64>,
    settled_total_cost_minor: Option<i64>,
    fee_received_minor: Option<i64>,
    accuracy: Option<String>,
    prediction_missing_input: Option<String>,
}

struct TimelineFacts {
    event: TimelineEventRow,
    emissions: Vec<LifecycleEmissionRow>,
    surfaces: Vec<TimelineSurfaceRow>,
    pace: TimelinePaceRow,
    latest_decision: Option<TimelineDecisionRow>,
    growth_actions: Vec<ShowGrowthActionRow>,
    assignments: Vec<TimelineAssignmentRow>,
    play_steps: Vec<TimelinePlayStepRow>,
    checklist: Vec<TimelineChecklistRow>,
    counts: TimelineCountsRow,
    harvest: TimelineHarvestRow,
    cost: Option<TimelineCostRow>,
}

#[derive(Debug, Serialize)]
struct TimelineActionView {
    kind: &'static str,
    label: &'static str,
}

#[derive(Debug, Serialize)]
struct TimelineStepView {
    key: &'static str,
    label: &'static str,
    /// Where the step sits on the ladder — an anchor off `starts_at`, not a
    /// timestamp, because "T-14" is how a band reads the week.
    anchor: &'static str,
    /// `done` the artifact completed; `active` work is in flight; `due` the
    /// window is open and the step needs doing; `waiting` the window has not
    /// opened; `skipped` the window closed without the step. There is no
    /// "unknown": a step with no evidence reports `due` once its window is
    /// open and `waiting` before it — absence is itself the state.
    state: &'static str,
    owner: Option<String>,
    action: Option<TimelineActionView>,
    detail: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct ControlPlaneEventTimelineResponse {
    event: TimelineEventView,
    steps: Vec<TimelineStepView>,
}

#[derive(Debug, Serialize)]
struct TimelineEventView {
    id: Uuid,
    slug: String,
    title: String,
    venue: Option<String>,
    status: String,
    starts_at: String,
    ends_at: Option<String>,
}

/// `GET /v1/control-plane/events/{event_slug}/timeline` — the nine-step
/// ladder for one show. Published and completed events both resolve: a
/// played night still owes the band T+1, T+3 and T+7.
pub async fn control_plane_event_timeline(
    State(state): State<crate::AppState>,
    Path(event_slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let facts = match crate::ops::hold(
        &state.read_budget,
        load_timeline_facts(&state.concert_qr, &event_slug),
    )
    .await
    {
        Ok(Some(facts)) => facts,
        Ok(None) => {
            return Problem::not_found(request_id_value)
                .private()
                .into_response();
        }
        Err(error) => {
            tracing::warn!(%error, "control-plane event timeline query failed");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };
    let steps = build_steps(&facts, OffsetDateTime::now_utc());
    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(ControlPlaneEventTimelineResponse {
            event: TimelineEventView {
                id: facts.event.id,
                slug: facts.event.slug.clone(),
                title: facts.event.title.clone(),
                venue: facts.event.venue.clone(),
                status: facts.event.status.clone(),
                starts_at: format_time(facts.event.starts_at),
                ends_at: facts.event.ends_at.map(format_time),
            },
            steps,
        }),
    )
        .into_response()
}

async fn load_timeline_facts(
    state: &ConcertQrState,
    event_slug: &str,
) -> Result<Option<TimelineFacts>, sqlx::Error> {
    let Some(event) = sqlx::query_as::<_, TimelineEventRow>(
        r#"
        SELECT id, slug, title, venue, status, starts_at, ends_at
        FROM events
        WHERE workspace_id = $1 AND slug = $2
          AND status IN ('published','completed')
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(event_slug)
    .fetch_optional(&state.database)
    .await?
    else {
        return Ok(None);
    };
    let workspace_id = state.workspace_id.into_uuid();
    let event_id = event.id;

    let emissions = sqlx::query_as::<_, LifecycleEmissionRow>(
        r#"
        SELECT phase, emitted_at
        FROM viryaos_campaign_lifecycle_emissions
        WHERE workspace_id = $1 AND event_id = $2
          AND phase = 'announcement'
        ORDER BY emitted_at
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_all(&state.database)
    .await?;

    let surfaces = sqlx::query_as::<_, TimelineSurfaceRow>(
        r#"
        SELECT surface_key, status
        FROM viryaos_show_growth_surfaces
        WHERE workspace_id = $1 AND event_id = $2
        ORDER BY surface_key
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_all(&state.database)
    .await?;

    // The same predicates the show-growth snapshot uses: an active sale's
    // paid and partially-refunded orders, capacity taken from the sale first
    // and the largest admission pool as the fallback — the brain's own
    // COALESCE(ticket_sale.capacity, admission.capacity).
    // The pace verdict itself stays in the brain's decisions — this surface
    // reports the numbers, not a re-derived opinion.
    let pace = sqlx::query_as::<_, TimelinePaceRow>(
        r#"
        SELECT COALESCE(ticket_sale.capacity, admission.capacity) AS capacity,
               COALESCE(ticket.paid_tickets, 0)::bigint AS paid_tickets,
               COALESCE(ticket.paid_tickets_last_7d, 0)::bigint AS paid_tickets_last_7d
        FROM events AS event
        LEFT JOIN LATERAL (
            SELECT MAX(sale.capacity)::bigint AS capacity
            FROM ticket_sales AS sale
            WHERE sale.workspace_id = event.workspace_id
              AND sale.event_id = event.id
              AND sale.active
        ) AS ticket_sale ON true
        LEFT JOIN LATERAL (
            SELECT SUM(item.quantity) FILTER (
                    WHERE orders.status IN ('paid','partially_refunded')
                )::bigint AS paid_tickets,
                SUM(item.quantity) FILTER (
                    WHERE orders.status IN ('paid','partially_refunded')
                      AND orders.paid_at >= now() - INTERVAL '7 days'
                )::bigint AS paid_tickets_last_7d
            FROM ticket_sales AS sale
            JOIN ticket_orders AS orders
              ON orders.workspace_id = sale.workspace_id
             AND orders.ticket_sale_id = sale.id
            JOIN ticket_order_items AS item
              ON item.workspace_id = orders.workspace_id
             AND item.ticket_order_id = orders.id
            WHERE sale.workspace_id = event.workspace_id
              AND sale.event_id = event.id
              AND sale.active
        ) AS ticket ON true
        LEFT JOIN LATERAL (
            SELECT MAX(pool.capacity)::bigint AS capacity
            FROM admission_pools AS pool
            WHERE pool.workspace_id = event.workspace_id
              AND pool.event_id = event.id
        ) AS admission ON true
        WHERE event.workspace_id = $1 AND event.id = $2
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_one(&state.database)
    .await?;

    let latest_decision = sqlx::query_as::<_, TimelineDecisionRow>(
        r#"
        SELECT reason, evaluated_at
        FROM viryaos_autopilot_decisions
        WHERE workspace_id = $1 AND context = 'show_growth' AND subject_id = $2
        ORDER BY evaluated_at DESC
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_optional(&state.database)
    .await?;

    let growth_actions = sqlx::query_as::<_, ShowGrowthActionRow>(
        r#"
        SELECT id, status, payload ->> 'lever' AS lever,
               available_at, finished_at
        FROM viryaos_autopilot_actions
        WHERE workspace_id = $1 AND context = 'show_growth' AND subject_id = $2
        ORDER BY created_at
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_all(&state.database)
    .await?;

    // Both assignment shapes key the event through `source_id`: a show_task
    // stores the event id there directly (source_ref names the checklist
    // item), an autopilot_action stores the action's subject — the event —
    // the same way. `source_ref` distinguishes the task inside the event.
    let assignments = sqlx::query_as::<_, TimelineAssignmentRow>(
        r#"
        SELECT assignment.source_kind, assignment.source_ref,
               assignment.action_id,
               assignment.status, assignment.due_at,
               member.display_name
        FROM viryaos_team_assignments AS assignment
        JOIN workspace_members AS member
          ON member.workspace_id = assignment.workspace_id
         AND member.id = assignment.assignee_member_id
        WHERE assignment.workspace_id = $1 AND assignment.source_id = $2
        ORDER BY assignment.due_at NULLS LAST
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_all(&state.database)
    .await?;

    let play_steps = sqlx::query_as::<_, TimelinePlayStepRow>(
        r#"
        SELECT step.step_kind, step.due_at, step.settled_at, step.skip_reason
        FROM viryaos_play_steps AS step
        JOIN viryaos_plays AS play
          ON play.workspace_id = step.workspace_id
         AND play.id = step.play_id
        WHERE step.workspace_id = $1
          AND play.anchor_kind = 'event'
          AND play.anchor_id = $2
        ORDER BY step.due_at
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_all(&state.database)
    .await?;

    let checklist = sqlx::query_as::<_, TimelineChecklistRow>(
        r#"
        SELECT item_key, status
        FROM show_checklist_items
        WHERE workspace_id = $1 AND event_id = $2
          AND item_key IN ('capture_plan','post_show_report')
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_all(&state.database)
    .await?;

    // Three counts in one round trip: the dedupe ledger proves a fan was
    // told, the campaign row proves the scan had a door, and the check-in
    // count is the scan itself. Delivered-vs-queued push detail stays on
    // the ops surface — the ladder needs the fact, not the funnel.
    let counts = sqlx::query_as::<_, TimelineCountsRow>(
        r#"
        SELECT
            (SELECT count(*)::bigint FROM nearby_gig_notifications
             WHERE workspace_id = $1 AND event_id = $2) AS nearby_notified,
            (SELECT count(*)::bigint FROM concert_qr_campaigns
             WHERE workspace_id = $1 AND event_id = $2
               AND active AND revoked_at IS NULL
               AND valid_until > now()) AS qr_campaigns,
            (SELECT count(*)::bigint FROM concert_checkins
             WHERE workspace_id = $1 AND event_id = $2) AS checkins
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_one(&state.database)
    .await?;

    // Harvest anchors on the content source the completed-show trigger
    // projects; artifact requests are content-supply actions whose subject is
    // that source. The pending predicate is the supply snapshot's own: an
    // action is in flight while it is open, or while it succeeded at the
    // request layer without a terminal execution report yet.
    let harvest = sqlx::query_as::<_, TimelineHarvestRow>(
        r#"
        SELECT
            (SELECT MAX(source.occurred_at)
             FROM viryaos_content_sources AS source
             WHERE source.workspace_id = $1
               AND source.source_kind = 'show_completed'
               AND source.source_key = 'show_completed:' || $2::text
               AND source.active
            ) AS occurred_at,
            (SELECT count(*)::bigint
             FROM viryaos_autopilot_actions AS action
             JOIN viryaos_content_sources AS source
               ON source.workspace_id = action.workspace_id
              AND source.id = action.subject_id
             WHERE action.workspace_id = $1
               AND action.context = 'content_supply'
               AND source.source_key = 'show_completed:' || $2::text
               AND source.active
               AND (
                   action.status IN ('awaiting_approval','queued','processing')
                   OR (
                       action.status = 'succeeded'
                       AND EXISTS (
                           SELECT 1
                           FROM viryaos_autopilot_action_emissions AS emission
                           WHERE emission.workspace_id = action.workspace_id
                             AND emission.action_id = action.id
                       )
                       AND NOT EXISTS (
                           SELECT 1
                           FROM viryaos_autopilot_execution_reports AS report
                           WHERE report.workspace_id = action.workspace_id
                             AND report.action_id = action.id
                             AND report.status IN ('succeeded','failed')
                       )
                   )
               )
            ) AS pending_requests,
            (SELECT count(*)::bigint
             FROM viryaos_autopilot_actions AS action
             JOIN viryaos_content_sources AS source
               ON source.workspace_id = action.workspace_id
              AND source.id = action.subject_id
             WHERE action.workspace_id = $1
               AND action.context = 'content_supply'
               AND source.source_key = 'show_completed:' || $2::text
               AND source.active
               AND action.status = 'succeeded'
               AND EXISTS (
                   SELECT 1
                   FROM viryaos_autopilot_execution_reports AS report
                   WHERE report.workspace_id = action.workspace_id
                     AND report.action_id = action.id
                     AND report.status = 'succeeded'
               )
            ) AS succeeded_requests
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_one(&state.database)
    .await?;

    let cost = sqlx::query_as::<_, TimelineCostRow>(
        r#"
        SELECT predicted_total_cost_minor, settled_total_cost_minor,
               fee_received_minor, accuracy, prediction_missing_input
        FROM viryaos_show_cost_ledger
        WHERE workspace_id = $1 AND event_id = $2
        ORDER BY predicted_at DESC
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_optional(&state.database)
    .await?;

    Ok(Some(TimelineFacts {
        event,
        emissions,
        surfaces,
        pace,
        latest_decision,
        growth_actions,
        assignments,
        play_steps,
        checklist,
        counts,
        harvest,
        cost,
    }))
}

fn task_owner<'a>(facts: &'a TimelineFacts, item_key: &str) -> Option<&'a str> {
    facts
        .assignments
        .iter()
        .find(|a| {
            a.status == "open" && a.source_kind == "show_task"
                && a.source_ref.as_deref() == Some(item_key)
        })
        .and_then(|a| a.display_name.as_deref())
}

/// The assignee of one specific open action. Owner slots name the person the
/// displayed action belongs to — an approver of an unrelated action must not
/// surface as the step's owner.
fn action_owner(facts: &TimelineFacts, action_id: Uuid) -> Option<&str> {
    facts
        .assignments
        .iter()
        .find(|a| {
            a.status == "open" && a.source_kind == "autopilot_action"
                && a.action_id == Some(action_id)
        })
        .and_then(|a| a.display_name.as_deref())
}

/// The pending lever that belongs to the pre-show window. Post-show levers
/// (recap, merch follow-up, follow ask) belong to the T+ steps — surfacing
/// one of them under "sales pace" would misattribute it.
fn pending_action(facts: &TimelineFacts) -> Option<&ShowGrowthActionRow> {
    facts
        .growth_actions
        .iter()
        .find(|a| {
            matches!(a.status.as_str(), "awaiting_approval" | "queued" | "processing")
                && !a
                    .lever
                    .as_deref()
                    .is_some_and(|lever| lever.starts_with("post_show"))
        })
}

fn checklist_status<'a>(facts: &'a TimelineFacts, item_key: &str) -> Option<&'a str> {
    facts
        .checklist
        .iter()
        .find(|c| c.item_key == item_key)
        .map(|c| c.status.as_str())
}

fn step(
    key: &'static str,
    label: &'static str,
    anchor: &'static str,
    state: &'static str,
    owner: Option<String>,
    action: Option<TimelineActionView>,
    detail: serde_json::Value,
) -> TimelineStepView {
    TimelineStepView {
        key,
        label,
        anchor,
        state,
        owner,
        action,
        detail,
    }
}

fn build_steps(facts: &TimelineFacts, now: OffsetDateTime) -> Vec<TimelineStepView> {
    let starts = facts.event.starts_at;
    let ends = facts.event.ends_at.unwrap_or(starts + Duration::hours(6));
    // Once the night has ended, a pre-show step with no evidence did not
    // fail — its window closed without it. `skipped`, never a stale `due`
    // offering "Announce it" for a show that already played.
    let show_over = now > ends;
    let mut steps = Vec::with_capacity(9);

    // T-21 — announced. Proof is the lifecycle emission; the surfaces say
    // where the announcement actually landed.
    let announced = facts.emissions.iter().find(|e| e.phase == "announcement");
    let announced_detail = serde_json::json!({
        "emitted_at": announced.map(|e| format_time(e.emitted_at)),
        "surfaces": facts
            .surfaces
            .iter()
            .map(|s| serde_json::json!({"surface": s.surface_key, "status": s.status}))
            .collect::<Vec<_>>(),
    });
    let (announced_state, announced_action) = if announced.is_some() {
        ("done", None)
    } else if show_over {
        ("skipped", None)
    } else if now >= starts - Duration::days(21) {
        ("due", Some(TimelineActionView { kind: "announce", label: "Announce it" }))
    } else {
        ("waiting", None)
    };
    steps.push(step("announced", "Announced", "T-21", announced_state, None, announced_action, announced_detail));

    // T-14 — sales pace. The numbers come from the same predicates the
    // brain's snapshot uses; the verdict comes from the brain's last
    // decision, quoted rather than re-derived.
    let pending = pending_action(facts);
    let pace_state = if facts.latest_decision.is_some() {
        "active"
    } else if show_over {
        "skipped"
    } else if now >= starts - Duration::days(14) {
        "due"
    } else {
        "waiting"
    };
    let pace_action = pending.map(|_| TimelineActionView {
        kind: "review",
        label: "Review the move the brain queued",
    });
    let pace_detail = serde_json::json!({
        "paid_tickets": facts.pace.paid_tickets,
        "capacity": facts.pace.capacity,
        "paid_tickets_last_7d": facts.pace.paid_tickets_last_7d,
        "last_read": facts.latest_decision.as_ref().map(|d| serde_json::json!({
            "reason": d.reason,
            "evaluated_at": format_time(d.evaluated_at),
        })),
        "pending_move": pending.map(|a| serde_json::json!({
            "lever": a.lever,
            "status": a.status,
        })),
    });
    let pace_owner = pending.and_then(|a| action_owner(facts, a.id));
    steps.push(step("sales_pace", "Sales pace", "T-14", pace_state, pace_owner.map(str::to_owned), pace_action, pace_detail));

    // T-7 — the bands posting. Plays carry the asks themselves. A skipped
    // step is settled but not done: an all-skipped play means nobody posted,
    // and the badge has to say so.
    let open_steps = facts
        .play_steps
        .iter()
        .filter(|s| s.settled_at.is_none())
        .count();
    let settled_steps = facts.play_steps.len() - open_steps;
    let successful_steps = facts
        .play_steps
        .iter()
        .filter(|s| s.settled_at.is_some() && s.skip_reason.is_none())
        .count();
    let posting_state = if open_steps > 0 && !show_over {
        "active"
    } else if successful_steps > 0 {
        "done"
    } else if settled_steps > 0 || show_over {
        "skipped"
    } else if now >= starts - Duration::days(7) {
        "due"
    } else {
        "waiting"
    };
    let posting_detail = serde_json::json!({
        "open": open_steps,
        "settled": settled_steps,
        "skipped": facts
            .play_steps
            .iter()
            .filter_map(|s| s.skip_reason.as_deref())
            .collect::<Vec<_>>(),
        "open_asks": facts
            .play_steps
            .iter()
            .filter(|s| s.settled_at.is_none())
            .map(|s| serde_json::json!({
                "kind": s.step_kind,
                "due_at": format_time(s.due_at),
            }))
            .collect::<Vec<_>>(),
        "next_assignment_due": facts
            .assignments
            .iter()
            .filter(|a| a.status == "open" && a.source_kind == "show_task")
            .filter_map(|a| a.due_at)
            .min()
            .map(format_time),
    });
    steps.push(step("bands_posting", "Bands posting", "T-7", posting_state, None, None, posting_detail));

    // T-2 — nearby fans. The dedupe ledger is the proof a fan was told; the
    // scheduler's 15-minute poll owns the send, so there is no human action
    // to offer — only the fact and the count.
    let nearby_state = if facts.counts.nearby_notified > 0 {
        "done"
    } else if show_over {
        "skipped"
    } else if now >= starts - Duration::days(2) {
        "due"
    } else {
        "waiting"
    };
    steps.push(step(
        "nearby_fans",
        "Nearby fans",
        "T-2",
        nearby_state,
        None,
        None,
        serde_json::json!({ "notified": facts.counts.nearby_notified }),
    ));

    // T-0 — the capture plan, owned by whoever the show_task handoff named.
    let capture_status = checklist_status(facts, "capture_plan").unwrap_or("pending");
    let capture_state = if capture_status == "done" {
        "done"
    } else if show_over {
        "skipped"
    } else if now >= starts - Duration::hours(30) {
        "due"
    } else {
        "waiting"
    };
    let capture_action = if capture_state == "due" {
        Some(TimelineActionView { kind: "capture_plan", label: "Confirm the capture plan" })
    } else {
        None
    };
    steps.push(step(
        "capture_plan",
        "Capture plan",
        "T-0",
        capture_state,
        task_owner(facts, "capture_plan").map(str::to_owned),
        capture_action,
        serde_json::json!({ "status": capture_status }),
    ));

    // T-0 — the scan itself. An active, unexpired campaign means the door
    // exists; check-ins are the night. The QR only becomes urgent inside the
    // last week — nagging about it three weeks out is noise. After the show
    // ends with no check-ins the window has closed: `skipped`, not failed.
    let scan_state = if facts.counts.checkins > 0 {
        "done"
    } else if show_over {
        "skipped"
    } else if facts.counts.qr_campaigns > 0 {
        "waiting"
    } else if now >= starts - Duration::days(7) {
        "due"
    } else {
        "waiting"
    };
    let scan_action = if scan_state == "due" || (scan_state == "waiting" && now >= starts - Duration::hours(6)) {
        Some(TimelineActionView { kind: "qr", label: "Open the QR" })
    } else {
        None
    };
    steps.push(step(
        "the_scan",
        "The scan",
        "T-0",
        scan_state,
        None,
        scan_action,
        serde_json::json!({
            "checkins": facts.counts.checkins,
            "campaign_ready": facts.counts.qr_campaigns > 0,
        }),
    ));

    // T+1 — the recall. The latest recap action is the artifact; the brain
    // anchors the window on `starts_at` (post_show_recap_hours = 30), so the
    // ladder does too. A cancelled or failed recap after the window closed
    // reads `skipped` — the request is not coming back on its own.
    let recap = facts
        .growth_actions
        .iter()
        .rev()
        .find(|a| a.lever.as_deref() == Some("post_show_recap"));
    let recap_window_open = now < starts + Duration::hours(30);
    let recap_state = match recap {
        Some(a) if a.status == "succeeded" => "done",
        Some(a) if matches!(a.status.as_str(), "awaiting_approval" | "queued" | "processing") => "active",
        _ if !recap_window_open => "skipped",
        Some(_) => "due",
        None => "waiting",
    };
    let recap_action = match recap {
        Some(a) if a.status == "awaiting_approval" => {
            Some(TimelineActionView { kind: "approve", label: "Approve the recap" })
        }
        _ => None,
    };
    steps.push(step(
        "recall",
        "Recall",
        "T+1",
        recap_state,
        recap.and_then(|a| action_owner(facts, a.id)).map(str::to_owned),
        recap_action,
        serde_json::json!({
            "action_status": recap.map(|a| a.status.as_str()),
            "send_after": recap.map(|a| format_time(a.available_at)),
            "finished_at": recap.and_then(|a| a.finished_at).map(format_time),
        }),
    ));

    // T+3 — the harvest. The content source appears when the night closes
    // and the brain holds artifact requests for post_show_harvest_hours
    // (72h) after `occurred_at`, so "source exists, nothing requested yet"
    // inside that window is `active`, not `done`. The window closing with
    // nothing pending and nothing collected is `skipped`.
    let harvest_state = if facts.harvest.pending_requests > 0 {
        "active"
    } else if let Some(occurred_at) = facts.harvest.occurred_at {
        if facts.harvest.succeeded_requests > 0 {
            "done"
        } else if now < occurred_at + Duration::hours(72) {
            "active"
        } else {
            "skipped"
        }
    } else if now >= ends + Duration::days(3) {
        "due"
    } else {
        "waiting"
    };
    steps.push(step(
        "harvest",
        "Harvest",
        "T+3",
        harvest_state,
        None,
        None,
        serde_json::json!({
            "occurred_at": facts.harvest.occurred_at.map(format_time),
            "pending_requests": facts.harvest.pending_requests,
            "collected_requests": facts.harvest.succeeded_requests,
        }),
    ));

    // T+7 — the numbers. The checklist row is the band's confirmation; the
    // cost ledger is what the night actually cost, when someone settled it.
    let report_status = checklist_status(facts, "post_show_report").unwrap_or("pending");
    let numbers_state = if report_status == "done" {
        "done"
    } else if now >= starts + Duration::days(7) {
        "due"
    } else {
        "waiting"
    };
    let numbers_action = if numbers_state == "due" {
        Some(TimelineActionView { kind: "report", label: "Write the report" })
    } else {
        None
    };
    let numbers_detail = serde_json::json!({
        "status": report_status,
        "cost": facts.cost.as_ref().map(|c| serde_json::json!({
            "predicted_total_cost_minor": c.predicted_total_cost_minor,
            "settled_total_cost_minor": c.settled_total_cost_minor,
            "fee_received_minor": c.fee_received_minor,
            "accuracy": c.accuracy,
            "prediction_missing_input": c.prediction_missing_input,
        })),
    });
    steps.push(step(
        "the_numbers",
        "The numbers",
        "T+7",
        numbers_state,
        // The report itself is system-owned; the human handoff on this step
        // is the post-show reconciliation the team index assigns.
        task_owner(facts, "post_show_reconciliation").map(str::to_owned),
        numbers_action,
        numbers_detail,
    ));

    steps
}
