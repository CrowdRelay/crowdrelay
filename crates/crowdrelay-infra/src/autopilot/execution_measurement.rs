/// The two counterfactual daily rates every fan-growth measurement trio is
/// planned against: the matched 14-day arrival rate and the aged durable
/// rate.
///
/// The rates are community-scoped when the action's experimental unit is a
/// community (the observation resolves the same handle through
/// `observable_community`, so the two sides of the subtraction count the same
/// population over the same width), and workspace-scoped otherwise.
///
/// The durable rate is *not* the arrival rate filtered to active fans. Y30's
/// outcome counts only arrivals that were still active thirty days later, so
/// its counterfactual has to be built from arrivals old enough to have had a
/// thirty-day durability outcome — the fourteen days ending thirty days ago.
/// Reusing the arrival rate here biases every Y30 estimate negative by
/// exactly the churn rate, which is how the brain used to learn that
/// everything it did was harmful.
async fn fan_growth_baselines(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
    now: OffsetDateTime,
) -> Result<(f64, f64), RepositoryError> {
    let community = measurement_unit_community(transaction, workspace_id, action_id).await?;
    let pre_action_daily_rate = if let Some(handle) = &community {
        sqlx::query_scalar::<_, f64>(
            r#"
            SELECT COUNT(DISTINCT fan_id)::double precision / 14.0
            FROM fan_provenance_events
            WHERE workspace_id = $1
              AND community = $3
              AND event_kind = 'conversion'
              AND fan_id IS NOT NULL
              AND occurred_at >= $2::timestamptz - INTERVAL '14 days'
              AND occurred_at < $2::timestamptz
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(now)
        .bind(handle)
        .fetch_one(&mut **transaction)
        .await
        .map_err(map_sqlx)?
    } else {
        sqlx::query_scalar::<_, f64>(
            r#"
            SELECT COUNT(*)::double precision / 14.0 FROM fans
            WHERE workspace_id = $1
              AND created_at >= $2::timestamptz - INTERVAL '14 days'
              AND created_at < $2::timestamptz
              AND status != 'suppressed'
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(now)
        .fetch_one(&mut **transaction)
        .await
        .map_err(map_sqlx)?
    };
    let pre_action_durable_daily_rate = if let Some(handle) = &community {
        sqlx::query_scalar::<_, f64>(
            r#"
            SELECT COUNT(DISTINCT fan.id)::double precision / 14.0
            FROM fan_provenance_events AS conversion
            JOIN fans AS fan
              ON fan.workspace_id = conversion.workspace_id
             AND fan.id = conversion.fan_id
            WHERE conversion.workspace_id = $1
              AND conversion.community = $3
              AND conversion.event_kind = 'conversion'
              AND conversion.occurred_at >= $2 - INTERVAL '44 days'
              AND conversion.occurred_at < $2 - INTERVAL '30 days'
              AND fan.status = 'active'
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(now)
        .bind(handle)
        .fetch_one(&mut **transaction)
        .await
        .map_err(map_sqlx)?
    } else {
        sqlx::query_scalar::<_, f64>(
            r#"
            SELECT COUNT(*)::double precision / 14.0 FROM fans
            WHERE workspace_id = $1
              AND created_at >= $2 - INTERVAL '44 days'
              AND created_at < $2 - INTERVAL '30 days'
              AND status = 'active'
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(now)
        .fetch_one(&mut **transaction)
        .await
        .map_err(map_sqlx)?
    };
    Ok((pre_action_daily_rate, pre_action_durable_daily_rate))
}

/// The community handle this action's experimental unit refers to, when it is
/// a community at all.
///
/// The unit id on a community assignment is an `agent_outreach_targets` UUID;
/// the ledger is keyed by the handle the smart link carries. Both the
/// counterfactual here and the observation in `observe_measurement` resolve it
/// the same way, so the two sides of the subtraction cannot drift onto
/// different keys.
async fn measurement_unit_community(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
) -> Result<Option<String>, RepositoryError> {
    let unit_id: Option<String> = sqlx::query_scalar::<_, String>(
        r#"
        SELECT unit_id
        FROM experiment_assignments
        WHERE workspace_id = $1
          AND action_id = $2
          AND unit_kind = 'target_community'
        LIMIT 1
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id.into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    let Some(unit_id) = unit_id else {
        return Ok(None);
    };
    let Ok(target_id) = Uuid::parse_str(&unit_id) else {
        return Ok(None);
    };
    sqlx::query_scalar::<_, String>(
        r#"
        SELECT display_name
        FROM agent_outreach_targets
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(target_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)
}


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
