async fn update_flag_inner(
    state: &crate::AppState,
    headers: &HeaderMap,
    key: &str,
    payload: UpdateFlagRequest,
) -> Result<FlagMutationResult, EcosystemError> {
    if flag_default(key).is_none()
        || payload
            .reason
            .as_deref()
            .is_some_and(|reason| reason.trim().len() > 500)
    {
        return Err(EcosystemError::BadRequest);
    }
    let idempotency_key = mutation_key(headers)?;
    let request_hash =
        hash_json(&json!({"key": key, "enabled": payload.enabled, "reason": payload.reason}));
    let target_id = deterministic_id("feature_flag", key);
    let mut tx = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(EcosystemError::sqlx)?;
    configure_transaction(&mut tx, state).await?;
    lock_mutation(&mut tx, state, &idempotency_key).await?;
    if let Some(existing) = existing_mutation(&mut tx, state, &idempotency_key).await? {
        validate_replay(
            &existing,
            "feature_flag.updated",
            "feature_flag",
            target_id,
            &request_hash,
        )?;
        let flag = load_flag_tx(&mut tx, state, key).await?;
        tx.commit().await.map_err(EcosystemError::sqlx)?;
        if let Some((key, _)) = flag_definition(key) {
            write_cached_flag(
                state.ticketing.workspace_id().into_uuid(),
                key,
                flag.enabled,
            )
            .await;
        }
        return Ok(FlagMutationResult {
            flag,
            replayed: true,
        });
    }
    sqlx::query(
        r#"
        INSERT INTO ecosystem_feature_flags (
            workspace_id, key, enabled, reason, updated_by_request_id
        ) VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (workspace_id, key) DO UPDATE
        SET enabled = EXCLUDED.enabled,
            reason = EXCLUDED.reason,
            version = ecosystem_feature_flags.version + 1,
            updated_at = now(),
            updated_by_request_id = EXCLUDED.updated_by_request_id
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(key)
    .bind(payload.enabled)
    .bind(
        payload
            .reason
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty()),
    )
    .bind(request_id(headers))
    .execute(&mut *tx)
    .await
    .map_err(EcosystemError::sqlx)?;
    append_action(
        &mut tx,
        state,
        "feature_flag.updated",
        "feature_flag",
        target_id,
        &idempotency_key,
        request_id(headers).as_deref(),
        &request_hash,
        json!({"key": key, "enabled": payload.enabled}),
    )
    .await?;
    let flag = load_flag_tx(&mut tx, state, key).await?;
    tx.commit().await.map_err(EcosystemError::sqlx)?;
    if let Some((key, _)) = flag_definition(key) {
        write_cached_flag(
            state.ticketing.workspace_id().into_uuid(),
            key,
            flag.enabled,
        )
        .await;
    }
    Ok(FlagMutationResult {
        flag,
        replayed: false,
    })
}
async fn reconcile_inner(
    state: &crate::AppState,
    headers: &HeaderMap,
    trigger: &str,
) -> Result<ReconciliationResult, EcosystemError> {
    if !matches!(trigger, "manual" | "scheduled" | "deploy" | "restore_drill") {
        return Err(EcosystemError::BadRequest);
    }
    let idempotency_key = mutation_key(headers)?;
    let request_hash = hash_json(&json!({"trigger": trigger}));
    let target_id = deterministic_id("reconciliation", &idempotency_key);
    let mut tx = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(EcosystemError::sqlx)?;
    configure_transaction(&mut tx, state).await?;
    lock_mutation(&mut tx, state, &idempotency_key).await?;
    if let Some(existing) = existing_mutation(&mut tx, state, &idempotency_key).await? {
        validate_replay(
            &existing,
            "reconciliation.run",
            "reconciliation",
            target_id,
            &request_hash,
        )?;
        let run_id = existing
            .details
            .get("run_id")
            .and_then(Value::as_str)
            .and_then(|value| Uuid::parse_str(value).ok())
            .ok_or(EcosystemError::Conflict)?;
        let result = load_reconciliation_tx(&mut tx, state, run_id).await?;
        tx.commit().await.map_err(EcosystemError::sqlx)?;
        return Ok(ReconciliationResult {
            replayed: true,
            ..result
        });
    }

    let run_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO reconciliation_runs (id, workspace_id, status, trigger, request_id)
        VALUES ($1, $2, 'running', $3, $4)
        "#,
    )
    .bind(run_id)
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(trigger)
    .bind(request_id(headers))
    .execute(&mut *tx)
    .await
    .map_err(EcosystemError::sqlx)?;

    insert_reconciliation_findings(&mut tx, state, run_id).await?;
    let finding_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM reconciliation_findings WHERE workspace_id = $1 AND run_id = $2",
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(run_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(EcosystemError::sqlx)?;
    let finding_count_i32 = i32::try_from(finding_count).map_err(|_| EcosystemError::Unexpected)?;
    sqlx::query(
        r#"
        UPDATE reconciliation_runs
        SET status = 'completed', finding_count = $3, finished_at = now()
        WHERE workspace_id = $1 AND id = $2 AND status = 'running'
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(run_id)
    .bind(finding_count_i32)
    .execute(&mut *tx)
    .await
    .map_err(EcosystemError::sqlx)?;

    sqlx::query(
        r#"
        INSERT INTO outbox_events (workspace_id, event_type, event_version, payload, request_id)
        SELECT finding.workspace_id,
               'reconciliation.finding_raised',
               1,
               jsonb_build_object(
                   'finding_id', finding.id,
                   'run_id', finding.run_id,
                   'kind', finding.kind,
                   'severity', finding.severity,
                   'entity_id', finding.entity_id,
                   'entity_label', finding.entity_label,
                   'summary', finding.summary,
                   'suggested_action', finding.suggested_action
               ),
               $3
        FROM reconciliation_findings AS finding
        WHERE finding.workspace_id = $1 AND finding.run_id = $2
          AND finding.severity IN ('warning', 'critical')
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(run_id)
    .bind(request_id(headers))
    .execute(&mut *tx)
    .await
    .map_err(EcosystemError::sqlx)?;

    append_action(
        &mut tx,
        state,
        "reconciliation.run",
        "reconciliation",
        target_id,
        &idempotency_key,
        request_id(headers).as_deref(),
        &request_hash,
        json!({"run_id": run_id, "finding_count": finding_count_i32, "trigger": trigger}),
    )
    .await?;
    let result = load_reconciliation_tx(&mut tx, state, run_id).await?;
    tx.commit().await.map_err(EcosystemError::sqlx)?;
    Ok(ReconciliationResult {
        replayed: false,
        ..result
    })
}

async fn insert_reconciliation_findings(
    tx: &mut Transaction<'_, Postgres>,
    state: &crate::AppState,
    run_id: Uuid,
) -> Result<(), EcosystemError> {
    sqlx::query(
        r#"
        INSERT INTO reconciliation_findings (
            workspace_id, run_id, kind, severity, entity_type, entity_id,
            entity_label, summary, suggested_action, metadata
        )
        SELECT $1, $2, 'ticket.pass_count_mismatch', 'critical', 'ticket_order',
               ticket_order.id, ticket_order.public_reference,
               'Paid ticket order does not have the expected number of admission passes',
               'inspect_ticket_order',
               jsonb_build_object('expected', expected.quantity, 'actual', actual.quantity)
        FROM ticket_orders AS ticket_order
        JOIN LATERAL (
            SELECT COALESCE(sum(item.quantity), 0)::bigint AS quantity
            FROM ticket_order_items AS item
            WHERE item.workspace_id = ticket_order.workspace_id
              AND item.ticket_order_id = ticket_order.id
        ) AS expected ON true
        JOIN LATERAL (
            SELECT count(pass.id)::bigint AS quantity
            FROM admission_passes AS pass
            JOIN ticket_order_items AS item
              ON item.workspace_id = pass.workspace_id
             AND item.id = pass.ticket_order_item_id
            WHERE item.workspace_id = ticket_order.workspace_id
              AND item.ticket_order_id = ticket_order.id
              AND pass.issuance_method = 'paid'
        ) AS actual ON true
        WHERE ticket_order.workspace_id = $1
          AND ticket_order.status IN ('paid', 'partially_refunded', 'refunded')
          AND expected.quantity <> actual.quantity

        UNION ALL

        SELECT $1, $2, 'ticket.paid_event_missing', 'warning', 'ticket_order',
               ticket_order.id, ticket_order.public_reference,
               'Paid ticket order has no durable ticket.order.paid outbox event',
               'inspect_outbox', '{}'::jsonb
        FROM ticket_orders AS ticket_order
        WHERE ticket_order.workspace_id = $1
          AND ticket_order.status IN ('paid', 'partially_refunded', 'refunded')
          AND NOT EXISTS (
              SELECT 1 FROM outbox_events AS event
              WHERE event.workspace_id = ticket_order.workspace_id
                AND event.event_type = 'ticket.order.paid'
                AND event.payload ->> 'order_id' = ticket_order.id::text
          )

        UNION ALL

        SELECT $1, $2, 'ticket.delivery_event_missing', 'warning', 'ticket_order',
               request.ticket_order_id, ticket_order.public_reference,
               'Ticket delivery request has no matching durable outbox event',
               'request_delivery_retry',
               jsonb_build_object('delivery_request_id', request.id)
        FROM ticket_delivery_requests AS request
        JOIN ticket_orders AS ticket_order
          ON ticket_order.workspace_id = request.workspace_id
         AND ticket_order.id = request.ticket_order_id
        WHERE request.workspace_id = $1
          AND NOT EXISTS (
              SELECT 1 FROM outbox_events AS event
              WHERE event.workspace_id = request.workspace_id
                AND event.event_type = 'ticket.order.delivery_requested'
                AND event.payload ->> 'order_id' = request.ticket_order_id::text
                AND event.created_at >= request.created_at - interval '5 seconds'
          )

        UNION ALL

        SELECT $1, $2, 'outbox.dead', 'critical', 'outbox_event', event.id,
               event.event_type,
               'Outbox event exhausted automatic retries', 'retry_outbox',
               jsonb_build_object('attempts', event.attempts, 'error_kind', event.last_error_kind)
        FROM outbox_events AS event
        WHERE event.workspace_id = $1 AND event.status = 'dead'

        UNION ALL

        SELECT $1, $2, 'webhook.dead', 'critical', 'webhook_delivery', delivery.id,
               endpoint.name,
               'Webhook delivery exhausted automatic retries', 'retry_delivery',
               jsonb_build_object(
                   'attempts', delivery.attempt_count,
                   'error_kind', delivery.last_error_kind,
                   'endpoint_active', endpoint.active
               )
        FROM webhook_deliveries AS delivery
        JOIN webhook_endpoints AS endpoint
          ON endpoint.workspace_id = delivery.workspace_id
         AND endpoint.id = delivery.endpoint_id
        WHERE delivery.workspace_id = $1 AND delivery.status = 'dead'
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(run_id)
    .execute(&mut **tx)
    .await
    .map_err(EcosystemError::sqlx)?;
    Ok(())
}

async fn load_reconciliation_tx(
    tx: &mut Transaction<'_, Postgres>,
    state: &crate::AppState,
    run_id: Uuid,
) -> Result<ReconciliationResult, EcosystemError> {
    let run = sqlx::query_as::<_, ReconciliationRun>(
        r#"
        SELECT id, status, trigger, finding_count, started_at, finished_at
        FROM reconciliation_runs
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(run_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(EcosystemError::sqlx)?;
    let findings = sqlx::query_as::<_, ReconciliationFinding>(
        r#"
        SELECT id, run_id, kind, severity, entity_type, entity_id,
               entity_label, summary, suggested_action, metadata,
               created_at, resolved_at
        FROM reconciliation_findings
        WHERE workspace_id = $1 AND run_id = $2
        ORDER BY severity DESC, created_at, id
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(run_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(EcosystemError::sqlx)?;
    Ok(ReconciliationResult {
        run,
        findings,
        replayed: false,
    })
}

async fn load_checklist(
    state: &crate::AppState,
    event_slug: &str,
) -> Result<ShowChecklist, EcosystemError> {
    let event = sqlx::query_as::<_, OverviewEvent>(
        r#"
        SELECT id, slug, title, venue, starts_at
        FROM events
        WHERE workspace_id = $1 AND slug = $2
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(event_slug)
    .fetch_optional(state.ticketing.pool())
    .await
    .map_err(EcosystemError::sqlx)?
    .ok_or(EcosystemError::NotFound)?;
    ensure_checklist_defaults(state, event.id).await?;
    let items = sqlx::query_as::<_, ChecklistItem>(
        r#"
        SELECT item_key, status, note, updated_at
        FROM show_checklist_items
        WHERE workspace_id = $1 AND event_id = $2
        ORDER BY item_key
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(event.id)
    .fetch_all(state.ticketing.pool())
    .await
    .map_err(EcosystemError::sqlx)?;
    Ok(ShowChecklist {
        event_id: event.id,
        event_slug: event.slug,
        event_title: event.title,
        starts_at: event.starts_at,
        items,
    })
}

async fn ensure_checklist_defaults(
    state: &crate::AppState,
    event_id: Uuid,
) -> Result<(), EcosystemError> {
    sqlx::query(
        r#"
        INSERT INTO show_checklist_items (workspace_id, event_id, item_key, status)
        SELECT $1, $2, defaults.item_key, 'pending'
        FROM (VALUES
            ('announcement_published'),
            ('ticketing_verified'),
            ('staff_assigned'),
            ('offline_snapshot_ready'),
            ('gate_device_charged'),
            ('backup_device_ready'),
            ('network_tested'),
            ('guestlist_checked'),
            ('post_show_reconciliation'),
            ('post_show_report')
        ) AS defaults(item_key)
        ON CONFLICT (workspace_id, event_id, item_key) DO NOTHING
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(event_id)
    .execute(state.ticketing.pool())
    .await
    .map_err(EcosystemError::sqlx)?;
    Ok(())
}

async fn update_checklist_inner(
    state: &crate::AppState,
    headers: &HeaderMap,
    event_slug: &str,
    item_key: &str,
    payload: UpdateChecklistRequest,
) -> Result<ShowChecklist, EcosystemError> {
    if !matches!(
        payload.status.as_str(),
        "pending" | "done" | "blocked" | "skipped"
    ) || item_key.is_empty()
        || item_key.len() > 64
        || payload
            .note
            .as_deref()
            .is_some_and(|note| note.len() > 1000)
    {
        return Err(EcosystemError::BadRequest);
    }
    let idempotency_key = mutation_key(headers)?;
    let request_hash = hash_json(&json!({
        "event_slug": event_slug,
        "item_key": item_key,
        "status": payload.status,
        "note": payload.note,
    }));
    let event = sqlx::query_as::<_, OverviewEvent>(
        "SELECT id, slug, title, venue, starts_at FROM events WHERE workspace_id = $1 AND slug = $2",
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(event_slug)
    .fetch_optional(state.ticketing.pool())
    .await
    .map_err(EcosystemError::sqlx)?
    .ok_or(EcosystemError::NotFound)?;
    let target_id = deterministic_id("checklist", &format!("{}:{item_key}", event.id));
    let mut tx = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(EcosystemError::sqlx)?;
    configure_transaction(&mut tx, state).await?;
    lock_mutation(&mut tx, state, &idempotency_key).await?;
    if let Some(existing) = existing_mutation(&mut tx, state, &idempotency_key).await? {
        validate_replay(
            &existing,
            "show_checklist.updated",
            "show_checklist",
            target_id,
            &request_hash,
        )?;
        tx.commit().await.map_err(EcosystemError::sqlx)?;
        return load_checklist(state, event_slug).await;
    }
    sqlx::query(
        r#"
        INSERT INTO show_checklist_items (
            workspace_id, event_id, item_key, status, note, updated_by_request_id
        ) VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (workspace_id, event_id, item_key) DO UPDATE
        SET status = EXCLUDED.status,
            note = EXCLUDED.note,
            updated_at = now(),
            updated_by_request_id = EXCLUDED.updated_by_request_id
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(event.id)
    .bind(item_key)
    .bind(payload.status)
    .bind(
        payload
            .note
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty()),
    )
    .bind(request_id(headers))
    .execute(&mut *tx)
    .await
    .map_err(EcosystemError::sqlx)?;
    append_action(
        &mut tx,
        state,
        "show_checklist.updated",
        "show_checklist",
        target_id,
        &idempotency_key,
        request_id(headers).as_deref(),
        &request_hash,
        json!({"event_id": event.id, "item_key": item_key}),
    )
    .await?;
    tx.commit().await.map_err(EcosystemError::sqlx)?;
    load_checklist(state, event_slug).await
}

async fn emit_due_inner(
    state: &crate::AppState,
    request_id_value: Option<&str>,
) -> Result<EmissionResult, EcosystemError> {
    let mut tx = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(EcosystemError::sqlx)?;
    configure_transaction(&mut tx, state).await?;
    let emitted = sqlx::query_scalar::<_, i64>(
        r#"
        WITH due AS (
            SELECT event.id AS event_id, event.title, event.starts_at,
                   CASE
                       WHEN event.starts_at BETWEEN now() + interval '6 days' AND now() + interval '8 days' THEN 'week'
                       WHEN event.starts_at BETWEEN now() + interval '18 hours' AND now() + interval '30 hours' THEN 'day'
                       WHEN event.starts_at BETWEEN now() + interval '90 minutes' AND now() + interval '3 hours' THEN 'gate'
                       WHEN event.starts_at BETWEEN now() - interval '8 hours' AND now() - interval '1 hour' THEN 'followup'
                   END AS phase
            FROM events AS event
            WHERE event.workspace_id = $1
              AND event.status IN ('published', 'completed')
        ), inserted_events AS (
            INSERT INTO outbox_events (workspace_id, event_type, event_version, payload, request_id)
            SELECT $1,
                   CASE WHEN due.phase = 'followup' THEN 'show.followup_due' ELSE 'show.checklist_due' END,
                   1,
                   jsonb_build_object(
                       'event_id', due.event_id,
                       'event_title', due.title,
                       'starts_at', due.starts_at,
                       'checklist', due.phase,
                       'severity', CASE WHEN due.phase = 'gate' THEN 'warning' ELSE 'info' END,
                       'summary', CASE due.phase
                           WHEN 'week' THEN 'Tydzień do koncertu: domknij sprzedaż, komunikację i obsadę.'
                           WHEN 'day' THEN 'Dzień do koncertu: pobierz snapshot offline i sprawdź guestlistę.'
                           WHEN 'gate' THEN 'Bramka zaraz rusza: urządzenia, backup i sieć muszą być gotowe.'
                           ELSE 'Po koncercie: uruchom reconciliation i raport wydarzenia.'
                       END
                   ),
                   $2
            FROM due
            WHERE due.phase IS NOT NULL
              AND NOT EXISTS (
                  SELECT 1 FROM show_notification_emissions AS emission
                  WHERE emission.workspace_id = $1
                    AND emission.event_id = due.event_id
                    AND emission.phase = due.phase
              )
            RETURNING id, payload
        ), emissions AS (
            INSERT INTO show_notification_emissions (
                workspace_id, event_id, phase, outbox_event_id
            )
            SELECT $1,
                   (payload ->> 'event_id')::uuid,
                   payload ->> 'checklist',
                   id
            FROM inserted_events
            RETURNING 1
        )
        SELECT count(*)::bigint FROM emissions
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(request_id_value)
    .fetch_one(&mut *tx)
    .await
    .map_err(EcosystemError::sqlx)?;
    tx.commit().await.map_err(EcosystemError::sqlx)?;
    Ok(EmissionResult { emitted })
}

async fn load_show_snapshot(
    state: &crate::AppState,
    event_slug: &str,
) -> Result<ShowModeSnapshot, EcosystemError> {
    let signing_key = state
        .ticketing
        .checkout_token_key()
        .ok_or(EcosystemError::Unavailable)?;
    let event = sqlx::query_as::<_, ShowEventRow>(
        r#"
        SELECT id, slug, title, venue, starts_at, doors_at, ends_at
        FROM events
        WHERE workspace_id = $1 AND slug = $2 AND status IN ('published', 'completed')
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(event_slug)
    .fetch_optional(state.ticketing.pool())
    .await
    .map_err(EcosystemError::sqlx)?
    .ok_or(EcosystemError::NotFound)?;
    let rows = sqlx::query_as::<_, ShowPassRow>(
        r#"
        SELECT pass.id AS pass_id, pass.public_reference, pass.holder_name,
               pass.holder_email, pass.issuance_method, pass.status,
               ticket_type.name AS ticket_type_name
        FROM admission_passes AS pass
        LEFT JOIN ticket_order_items AS item
          ON item.workspace_id = pass.workspace_id AND item.id = pass.ticket_order_item_id
        LEFT JOIN ticket_types AS ticket_type
          ON ticket_type.workspace_id = item.workspace_id AND ticket_type.id = item.ticket_type_id
        WHERE pass.workspace_id = $1 AND pass.event_id = $2
          AND pass.status IN ('claimed', 'redeemed')
        ORDER BY pass.public_reference
        LIMIT $3
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(event.id)
    .bind(MAX_SHOW_PASSES + 1)
    .fetch_all(state.ticketing.pool())
    .await
    .map_err(EcosystemError::sqlx)?;
    if i64::try_from(rows.len()).map_err(|_| EcosystemError::Unexpected)? > MAX_SHOW_PASSES {
        return Err(EcosystemError::Conflict);
    }
    let generated_at = OffsetDateTime::now_utc();
    let qr_not_before = event
        .doors_at
        .unwrap_or(event.starts_at)
        .saturating_sub(TimeDuration::hours(6));
    let qr_expires_at = event
        .ends_at
        .unwrap_or_else(|| event.starts_at.saturating_add(TimeDuration::hours(12)))
        .checked_add(TimeDuration::hours(24))
        .ok_or(EcosystemError::Unexpected)?;
    let expires_at = std::cmp::min(
        qr_expires_at,
        generated_at.saturating_add(TimeDuration::hours(48)),
    );
    if expires_at <= generated_at {
        return Err(EcosystemError::Conflict);
    }
    let mut passes = Vec::with_capacity(rows.len());
    for row in rows {
        let offline_eligible = row.issuance_method == "paid" && row.status == "claimed";
        let qr_sha256 = if offline_eligible {
            let token = encode_ticket_qr(
                row.pass_id,
                event.id,
                &row.public_reference,
                qr_not_before.unix_timestamp(),
                qr_expires_at.unix_timestamp(),
                &signing_key,
            )
            .map_err(|_| EcosystemError::Unexpected)?;
            Some(hex::encode(Sha256::digest(token.as_bytes())))
        } else {
            None
        };
        passes.push(ShowModePass {
            public_reference: row.public_reference,
            holder_name: row.holder_name,
            holder_email_masked: mask_email(row.holder_email.as_deref()),
            ticket_type_name: row.ticket_type_name,
            offline_eligible,
            qr_sha256,
        });
    }
    let event_view = ShowModeEvent {
        slug: event.slug,
        title: event.title,
        venue: event.venue,
        starts_at: format_time(event.starts_at)?,
    };
    let generated_at_text = format_time(generated_at)?;
    let expires_at_text = format_time(expires_at)?;
    let snapshot_id = Uuid::now_v7().to_string();
    let checksum_sha256 = snapshot_checksum(
        &snapshot_id,
        &event_view,
        &generated_at_text,
        &expires_at_text,
        &passes,
    );
    Ok(ShowModeSnapshot {
        schema_version: SHOW_SNAPSHOT_SCHEMA,
        snapshot_id,
        event: event_view,
        generated_at: generated_at_text,
        expires_at: expires_at_text,
        checksum_sha256,
        passes,
    })
}

fn snapshot_checksum(
    snapshot_id: &str,
    event: &ShowModeEvent,
    generated_at: &str,
    expires_at: &str,
    passes: &[ShowModePass],
) -> String {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, "crowdrelay/show-mode/v1");
    hash_field(&mut hasher, &SHOW_SNAPSHOT_SCHEMA.to_string());
    hash_field(&mut hasher, snapshot_id);
    hash_field(&mut hasher, &event.slug);
    hash_field(&mut hasher, &event.title);
    hash_field(&mut hasher, event.venue.as_deref().unwrap_or(""));
    hash_field(&mut hasher, &event.starts_at);
    hash_field(&mut hasher, generated_at);
    hash_field(&mut hasher, expires_at);
    // The snapshot query orders by public_reference, so hashing can stream
    // directly without a second 10k-entry allocation and O(n log n) sort.
    for pass in passes {
        hash_field(&mut hasher, &pass.public_reference);
        hash_field(&mut hasher, pass.holder_name.as_deref().unwrap_or(""));
        hash_field(&mut hasher, &pass.holder_email_masked);
        hash_field(&mut hasher, pass.ticket_type_name.as_deref().unwrap_or(""));
        hash_field(&mut hasher, if pass.offline_eligible { "1" } else { "0" });
        hash_field(&mut hasher, pass.qr_sha256.as_deref().unwrap_or(""));
    }
    hex::encode(hasher.finalize())
}

fn hash_field(hasher: &mut Sha256, value: &str) {
    hasher.update(value.len().to_be_bytes());
    hasher.update(value.as_bytes());
}

fn format_time(value: OffsetDateTime) -> Result<String, EcosystemError> {
    value
        .format(&Rfc3339)
        .map_err(|_| EcosystemError::Unexpected)
}
