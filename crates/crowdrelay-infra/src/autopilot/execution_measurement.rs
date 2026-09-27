/// Plans the attendance measurement for an event-bound action.
///
/// Attendance is the show's own outcome — every lever and every campaign
/// works the same event, so each answers for whether the room filled.
/// Anchored at the event's start plus a fortnight, not the dispatch: a
/// lever that runs a month out still reads the settled count, and a
/// post-show lever reads it as soon as it exists. A cancelled show has no
/// attendance to observe — the observation arm refuses it again there, and
/// skipping here keeps a dead measurement out of the queue in the first
/// place.
async fn schedule_attendance(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    event_id: uuid::Uuid,
    plans: &mut Vec<(
        AutopilotMeasurementKind,
        uuid::Uuid,
        f64,
        OffsetDateTime,
    )>,
) -> Result<(), RepositoryError> {
    let event_window: Option<(OffsetDateTime, String)> = sqlx::query_as(
        r#"
        SELECT starts_at, status
        FROM events
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(event_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    if let Some((starts_at, status)) = event_window
        && status != "cancelled"
    {
        plans.push((
            AutopilotMeasurementKind::ShowAttendanceRate14d,
            event_id,
            0.0,
            starts_at + time::Duration::days(14),
        ));
    }
    Ok(())
}

/// Paid gross across a ticket type in the 72 hours before the price change —
/// the `TicketRevenue72h` counterfactual baseline.
async fn audience_ticket_revenue_baseline_72h(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    event_id: &EventId,
    now: OffsetDateTime,
) -> Result<f64, RepositoryError> {
    sqlx::query_scalar::<_, f64>(
        r#"
        SELECT COALESCE(SUM(ticket_order.amount_gross_minor),0)::double precision
        FROM ticket_orders ticket_order
        JOIN ticket_sales sale
          ON sale.workspace_id=ticket_order.workspace_id AND sale.id=ticket_order.ticket_sale_id
        WHERE ticket_order.workspace_id=$1 AND sale.event_id=$2
          AND ticket_order.status IN ('paid','partially_refunded','refunded')
          AND ticket_order.paid_at >= $3 - INTERVAL '72 hours'
          AND ticket_order.paid_at < $3
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(event_id.into_uuid())
    .bind(now)
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)
}

async fn ticket_revenue_baseline_72h(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    ticket_type_id: &TicketTypeId,
    now: OffsetDateTime,
) -> Result<f64, RepositoryError> {
    sqlx::query_scalar::<_, f64>(
        r#"
        SELECT COALESCE(SUM(item.total_gross_minor), 0)::double precision
        FROM ticket_order_items AS item
        JOIN ticket_orders AS ticket_order
          ON ticket_order.workspace_id = item.workspace_id
         AND ticket_order.id = item.ticket_order_id
        WHERE item.workspace_id = $1
          AND item.ticket_type_id = $2
          AND ticket_order.status = 'paid'
          AND ticket_order.paid_at >= $3 - INTERVAL '72 hours'
          AND ticket_order.paid_at < $3
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(ticket_type_id.into_uuid())
    .bind(now)
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)
}

/// One approach's `BookingAgentReply30d` plan, keyed to the agent's own id.
/// A reply answers that agent's ask, not the batch's; lumping them under the
/// wave would let one answer count N times.
fn wave_reply_measurement(
    approach: &crowdrelay_application::autopilot::BookingAgentApproachDraft,
    now: OffsetDateTime,
) -> (AutopilotMeasurementKind, uuid::Uuid, f64, OffsetDateTime) {
    (
        AutopilotMeasurementKind::BookingAgentReply30d,
        approach.agent_id.into_uuid(),
        0.0,
        now + time::Duration::days(30),
    )
}


/// New Signal installs in the `days` before `now`, counted exactly the way
/// the install observers count the window after it: active, not
/// invalidated, created inside the window.
///
/// The install measurements compare two windows of the same width. The
/// agent-run arm used to pass the standing install total instead, so the
/// comparison was "installs this week" against "installs ever": with three
/// endpoints on record every dispatch that added none read −100%, and one
/// that added two still read −33%. Production scored 43 of 87 worsened
/// outcomes that way in the week to 2026-09-27. The Signal push arm had the
/// same fault in a smaller form — its one-day checkpoint was compared
/// against a seven-day pre-window.
async fn pre_action_signal_installs(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
    days: i32,
) -> Result<f64, RepositoryError> {
    sqlx::query_scalar::<_, f64>(
        r#"
        SELECT COUNT(*)::double precision
        FROM fan_push_endpoints
        WHERE workspace_id = $1
          AND active = true
          AND invalidated_at IS NULL
          AND created_at >= $2::timestamptz - make_interval(days => $3)
          AND created_at < $2::timestamptz
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .bind(days)
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)
}
