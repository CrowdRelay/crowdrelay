async fn update_flag_inner(
    state: &crate::AppState,
    headers: &HeaderMap,
    key: &str,
    payload: UpdateFlagRequest,
) -> Result<FlagMutationResult, EcosystemError> {
    // The HTTP layer still owns input shape and the declared flag set; the
    // transaction, replay window and audit row belong to the repository.
    if flag_default(key).is_none()
        || payload
            .reason
            .as_deref()
            .is_some_and(|reason| reason.trim().len() > 500)
    {
        return Err(EcosystemError::BadRequest);
    }
    let command = UpdateFeatureFlagCommand {
        workspace_id: state.ticketing.workspace_id(),
        key: key.to_owned(),
        enabled: payload.enabled,
        reason: payload.reason,
        idempotency_key: mutation_key(headers)?,
        request_id: request_id(headers),
    };
    let mutation = state.ecosystem.update_feature_flag(&command).await?;
    if let Some((key, _)) = flag_definition(key) {
        write_cached_flag(
            state.ticketing.workspace_id().into_uuid(),
            key,
            mutation.flag.enabled,
        )
        .await;
    }
    Ok(FlagMutationResult {
        flag: FeatureFlag {
            key: mutation.flag.key,
            enabled: mutation.flag.enabled,
            reason: mutation.flag.reason,
            version: mutation.flag.version,
            updated_at: mutation.flag.updated_at,
        },
        replayed: mutation.replayed,
    })
}

async fn reconcile_inner(
    state: &crate::AppState,
    headers: &HeaderMap,
    trigger: &str,
) -> Result<ReconciliationResult, EcosystemError> {
    // The trigger vocabulary is HTTP input policy; the pass itself, its findings
    // and its replay window are the repository's.
    if !matches!(trigger, "manual" | "scheduled" | "deploy" | "restore_drill") {
        return Err(EcosystemError::BadRequest);
    }
    let command = RunReconciliationCommand {
        workspace_id: state.ticketing.workspace_id(),
        trigger: trigger.to_owned(),
        idempotency_key: mutation_key(headers)?,
        request_id: request_id(headers),
    };
    let outcome = state.ecosystem.run_reconciliation(&command).await?;
    Ok(ReconciliationResult {
        run: ReconciliationRun {
            id: outcome.run.id,
            status: outcome.run.status,
            trigger: outcome.run.trigger,
            finding_count: outcome.run.finding_count,
            started_at: outcome.run.started_at,
            finished_at: outcome.run.finished_at,
        },
        findings: outcome
            .findings
            .into_iter()
            .map(|finding| ReconciliationFinding {
                id: finding.id,
                run_id: finding.run_id,
                kind: finding.kind,
                severity: finding.severity,
                entity_type: finding.entity_type,
                entity_id: finding.entity_id,
                entity_label: finding.entity_label,
                summary: finding.summary,
                suggested_action: finding.suggested_action,
                metadata: finding.metadata,
                created_at: finding.created_at,
                resolved_at: finding.resolved_at,
            })
            .collect(),
        replayed: outcome.replayed,
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
        SELECT item_key, section, sort_order, status, note, updated_at
        FROM show_checklist_items
        WHERE workspace_id = $1 AND event_id = $2
        ORDER BY sort_order, item_key
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
        INSERT INTO show_checklist_items (
            workspace_id, event_id, item_key, section, sort_order, status
        )
        SELECT $1, $2, defaults.item_key, defaults.section, defaults.sort_order, 'pending'
        FROM (VALUES
            ('laptop_charged_packed', 'gear', 10),
            ('setlist_ready', 'show_files', 20),
            ('show_files_backup_ready', 'show_files', 30),
            ('merch_packed', 'gear', 40),
            ('rack_cables_instruments_packed', 'gear', 50),
            ('instrument_spares_packed', 'gear', 60),
            ('stage_outfit_packed', 'gear', 70),
            ('wireless_checked', 'gear', 80),
            ('power_and_chargers_packed', 'gear', 90),
            ('camera_handoff_ready', 'media', 110),
            ('venue_schedule_confirmed', 'logistics', 130),
            ('tech_rider_confirmed', 'logistics', 140),
            ('staff_assigned', 'logistics', 150),
            ('guestlist_checked', 'logistics', 160),
            ('offline_snapshot_ready', 'gate', 210),
            ('gate_device_charged', 'gate', 220),
            ('backup_device_ready', 'gate', 230),
            ('network_tested', 'gate', 240),
            ('post_show_reconciliation', 'post_show', 310),
            ('post_show_report', 'post_show', 320)
        ) AS defaults(item_key, section, sort_order)
        ON CONFLICT (workspace_id, event_id, item_key) DO UPDATE
        SET section = EXCLUDED.section,
            sort_order = EXCLUDED.sort_order
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
    // Which statuses and key shapes are legal is HTTP input policy; the
    // transaction, replay window and audit row belong to the repository.
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
    let command = UpdateShowChecklistCommand {
        workspace_id: state.ticketing.workspace_id(),
        event_slug: event_slug.to_owned(),
        item_key: item_key.to_owned(),
        status: payload.status,
        note: payload.note,
        idempotency_key: mutation_key(headers)?,
        request_id: request_id(headers),
    };
    state.ecosystem.update_show_checklist(&command).await?;
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
    let emitted = sqlx::query_scalar::<_, i64>(
        r#"
        WITH due AS (
            SELECT event.id AS event_id, event.slug AS event_slug, event.title, event.starts_at,
                   CASE
                       WHEN event.starts_at BETWEEN now() + interval '6 days 18 hours' AND now() + interval '7 days' THEN 'week'
                       WHEN event.starts_at BETWEEN now() + interval '42 hours' AND now() + interval '48 hours' THEN 'two_days'
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
                       'event_slug', due.event_slug,
                       'event_title', due.title,
                       'starts_at', due.starts_at,
                       'checklist', due.phase,
                       'severity', CASE WHEN due.phase = 'gate' THEN 'warning' ELSE 'info' END,
                       'summary', CASE due.phase
                           WHEN 'week' THEN 'Tydzień do koncertu: domknij sprzedaż, komunikację i obsadę.'
                           WHEN 'two_days' THEN 'Dwa dni do koncertu: domknij checklistę sprzętu, plików, merchu i stroju.'
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
            RETURNING id, workspace_id, payload
        ), inserted_push AS (
            INSERT INTO fan_push_deliveries (
                workspace_id, fan_id, audience_kind, endpoint_id,
                source_kind, source_id, category, title, body, target_path,
                collapse_key, status, available_at
            )
            SELECT inserted.workspace_id,
                   NULL,
                   'staff',
                   endpoint.id,
                   'show_checklist',
                   inserted.id,
                   'staff',
                   CASE inserted.payload ->> 'checklist'
                       WHEN 'week' THEN 'VIRYA · koncert za 7 dni'
                       WHEN 'two_days' THEN 'VIRYA · koncert za 2 dni'
                       ELSE 'VIRYA · checklista koncertowa'
                   END,
                   (inserted.payload ->> 'event_title') || ' — otwórz checklistę i odhacz przygotowania.',
                   '/staff/checklist?event=' || (inserted.payload ->> 'event_slug'),
                   'show-checklist:' || (inserted.payload ->> 'event_id') || ':' || (inserted.payload ->> 'checklist'),
                   'queued',
                   now()
            FROM inserted_events inserted
            JOIN fan_push_endpoints endpoint
              ON endpoint.workspace_id = inserted.workspace_id
             AND endpoint.audience_kind = 'staff'
             AND endpoint.active
             AND endpoint.invalidated_at IS NULL
            WHERE inserted.payload ->> 'checklist' IN ('week', 'two_days')
            ON CONFLICT (workspace_id, source_kind, source_id, endpoint_id) DO NOTHING
            RETURNING 1
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
