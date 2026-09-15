// The gig page's one night, as data: every workspace-scoped artifact row the
// T-21→T+7 ladder reads. `timeline.rs` turns these facts into the nine
// steps; this file owns the row shapes and the loader.
//
// Every query here reads an existing workspace-scoped artifact table;
// nothing in this file writes, and nothing fetched carries a campaign
// credential or a per-fan row. Missing evidence is null, not zero — a
// measurement that does not exist yet loads as no row, not a fabricated
// count.

#[derive(Debug, FromRow)]
struct TimelineEventRow {
    id: Uuid,
    slug: String,
    title: String,
    venue: Option<String>,
    venue_address: Option<String>,
    status: String,
    starts_at: OffsetDateTime,
    ends_at: Option<OffsetDateTime>,
}

#[derive(Debug, FromRow)]
struct TimelineActRow {
    act_slug: String,
    act_name: String,
}

#[derive(Debug, FromRow)]
struct TimelineCrossbillEdgeRow {
    max_campaigns_per_month: i16,
    cooldown_days: i16,
    deliveries_this_month: i64,
}

#[derive(Debug, FromRow)]
struct TimelineBeaconRow {
    display_name: String,
    beacon_kind: String,
    status: String,
    last_reply_disposition: String,
    last_outreach_at: Option<OffsetDateTime>,
    notes: Option<String>,
}

#[derive(Debug, FromRow)]
struct TimelineRecapCampaignRow {
    slug: String,
    subject: Option<String>,
    status: String,
    scheduled_at: Option<OffsetDateTime>,
    delivered_count: Option<i32>,
    recipient_count: Option<i32>,
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
    crossbill_acts: Vec<TimelineActRow>,
    crossbill_edge: Option<TimelineCrossbillEdgeRow>,
    venue_beacons: Vec<TimelineBeaconRow>,
    recap_campaign: Option<TimelineRecapCampaignRow>,
}

async fn load_timeline_facts(
    state: &ConcertQrState,
    event_slug: &str,
) -> Result<Option<TimelineFacts>, sqlx::Error> {
    let Some(event) = sqlx::query_as::<_, TimelineEventRow>(
        r#"
        SELECT id, slug, title, venue, venue_address, status, starts_at, ends_at
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

    // The shared bill is the T-21 artifact: who else plays, and whether an
    // active crossbill edge lets the system push the night to a bill-mate's
    // audience. The cap travels with it — the consent's monthly ceiling and
    // cooldown are the honest bound, not a made-up per-show limit.
    let crossbill_acts = sqlx::query_as::<_, TimelineActRow>(
        r#"
        SELECT act_slug, act_name
        FROM event_acts
        WHERE workspace_id = $1 AND event_id = $2
        ORDER BY position, act_slug
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_all(&state.database)
    .await?;

    // Only an edge whose beneficiary is this workspace amplifies its shows —
    // `from` owns the audience, `to` benefits (same direction rule as
    // `event_crossbill` in fan_context). The cap counts CAMPAIGNS, not fan
    // rows — `count(DISTINCT campaign_reference)` matches the enforcement
    // count in infra/portfolio.rs exactly, same calendar-month window.
    let crossbill_edge = if facts_crossbill_needed(&crossbill_acts) {
        sqlx::query_as::<_, TimelineCrossbillEdgeRow>(
            r#"
            SELECT edge.max_campaigns_per_month, edge.cooldown_days,
                (SELECT count(DISTINCT d.campaign_reference)
                 FROM amplification_deliveries AS d
                 WHERE d.consent_id = edge.id AND d.to_workspace_id = $1
                   AND d.delivered_at >= date_trunc('month', now())) AS deliveries_this_month
            FROM amplification_consents AS edge
            WHERE edge.to_workspace_id = $1
              AND edge.purpose = 'event_crossbill'
              AND edge.status = 'active'
            ORDER BY edge.created_at
            LIMIT 1
            "#,
        )
        .bind(workspace_id)
        .fetch_optional(&state.database)
        .await?
    } else {
        None
    };

    // Venue knowledge lives on the show: the beacon-campaign record keyed
    // by event_id — who the room is to us (relationship status, the last
    // reply's disposition, the operator's notes). No fuzzy venue matching.
    let venue_beacons = sqlx::query_as::<_, TimelineBeaconRow>(
        r#"
        SELECT beacon.display_name, beacon.beacon_kind,
               campaign.status, campaign.last_reply_disposition,
               campaign.last_outreach_at, campaign.notes
        FROM viryaos_beacon_campaigns AS campaign
        JOIN viryaos_beacons AS beacon
          ON beacon.workspace_id = campaign.workspace_id
         AND beacon.id = campaign.beacon_id
        WHERE campaign.workspace_id = $1 AND campaign.event_id = $2
        ORDER BY campaign.last_outreach_at DESC NULLS LAST, beacon.display_name
        LIMIT 8
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_all(&state.database)
    .await?;

    // The T+1 recall's artifact is the recap campaign itself — tagged by the
    // same `content->>'event_id'` link the report's campaign read uses, with
    // the lever narrowing it to the recap rather than any event send.
    let recap_campaign = sqlx::query_as::<_, TimelineRecapCampaignRow>(
        r#"
        -- The recap INSERT never sets `subject`; `name` is the populated
        -- human label, so the detail falls back to it rather than always
        -- reading null.
        SELECT slug, COALESCE(subject, name) AS subject, status,
               scheduled_at, delivered_count, recipient_count
        FROM communication_campaigns
        WHERE workspace_id = $1 AND content->>'event_id' = $2
          AND content->>'lever' = 'post_show_recap'
        ORDER BY created_at DESC
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .bind(event_id.to_string())
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
        crossbill_acts,
        crossbill_edge,
        venue_beacons,
        recap_campaign,
    }))
}

/// The edge lookup only matters when a bill exists to share — a solo show
/// never has a crossbill, so the consent edge is not worth a query.
fn facts_crossbill_needed(acts: &[TimelineActRow]) -> bool {
    acts.len() > 1
}

