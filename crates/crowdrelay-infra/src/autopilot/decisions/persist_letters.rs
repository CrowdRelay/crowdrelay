/// Composes the booking letter onto a `RequestBookingOutreach` payload while
/// the action is still being written.
///
/// The evaluator cannot write the letter: it is pure and the sender identity,
/// the city's name and its country all live in Postgres. Here is where the
/// transaction that writes the action exists, so the draft is composed here —
/// the approval reads the same words the target will. A refusal leaves the
/// draft empty: dispatch fails closed on it, and the briefing prints "not
/// composed" rather than words nobody approved.
async fn enrich_booking_draft(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action: &mut AutopilotActionPayload,
) -> Result<(), RepositoryError> {
    use crowdrelay_domain::booking_letter::{compose_booking_letter, BookingLetterInput};
    use crowdrelay_domain::gig_letter::LetterLanguage;
    use crowdrelay_domain::venue_evidence::{EvidenceLocale, booking_evidence_line};

    let AutopilotActionPayload::RequestBookingOutreach {
        draft,
        city_id,
        target_name,
        proposed_window,
        venue_evidence,
        phase,
        ..
    } = action
    else {
        return Ok(());
    };

    let ws = workspace_id.into_uuid();
    let city = sqlx::query_as::<_, (String, String)>(
        "SELECT name, country_code FROM cities WHERE id = $1",
    )
    .bind((*city_id).into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    let Some((city_name, country_code)) = city else {
        // A city the action names must exist — the candidate carried its id.
        return Err(RepositoryError::NotFound);
    };
    let language = LetterLanguage::for_country(&country_code);
    let sender = sender_identity_in_tx(transaction, ws).await?;
    let locale = match language {
        LetterLanguage::Polish => EvidenceLocale::Pl,
        _ => EvidenceLocale::En,
    };
    let first_line_fact = venue_evidence
        .as_ref()
        .and_then(|evidence| booking_evidence_line(evidence, locale));
    let window = proposed_window
        .as_ref()
        .map(|window| (window.start, window.end));
    if let Ok(letter) = compose_booking_letter(&BookingLetterInput {
        language,
        sender: &sender,
        target_name,
        city: &city_name,
        proposed_window: window,
        first_line_fact: first_line_fact.as_deref(),
        phase: *phase,
    }) {
        *draft = letter;
    }
    Ok(())
}

/// The sender half of every letter — act name, declared style, the city the
/// act calls home and its own site. Shared by the booking and outreach
/// enrichers; reads run inside the caller's transaction so the letter is
/// composed from the same snapshot the action is written against.
///
/// `tenant_settings` holds only the stored override: a default site URL in a
/// stranger's inbox is a link to somebody else's website, so an unset field
/// shortens the sentence instead of borrowing one.
async fn sender_identity_in_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: uuid::Uuid,
) -> Result<crowdrelay_domain::gig_letter::SenderIdentity, RepositoryError> {
    Ok(crowdrelay_domain::gig_letter::SenderIdentity {
        act_name: sqlx::query_scalar::<_, String>("SELECT name FROM workspaces WHERE id = $1")
            .bind(workspace_id)
            .fetch_optional(&mut **transaction)
            .await
            .map_err(map_sqlx)?
            .unwrap_or_default(),
        style: sqlx::query_scalar::<_, String>(
            "SELECT value FROM tenant_settings WHERE workspace_id = $1 AND key = 'act_style'",
        )
        .bind(workspace_id)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(map_sqlx)?
        .as_deref()
        .and_then(crowdrelay_domain::gig_letter::letter_style),
        // The act's declared home city (`act_home_city`), trimmed like
        // `site_url`: never measured — the most-played-city query it replaced
        // counted upcoming shows as played and named the act after the city
        // of its next gig. Unset means no city in the sentence.
        home_city: sqlx::query_scalar::<_, String>(
            "SELECT value FROM tenant_settings WHERE workspace_id = $1 AND key = 'act_home_city'",
        )
        .bind(workspace_id)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(map_sqlx)?
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty()),
        site_url: sqlx::query_scalar::<_, String>(
            "SELECT value FROM tenant_settings WHERE workspace_id = $1 AND key = 'member_site_base_url'",
        )
        .bind(workspace_id)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(map_sqlx)?
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty()),
    })
}

/// Composes the outreach pitch onto a `RequestOutreach` payload while the
/// action is still being written — the same rule as the booking letter.
///
/// The pitch is the tenant's most recent active release plan that carries a
/// listen link, else its newest dated album, EP or single from the synced
/// catalogue (`outreach_supply::outreach_pitch`); with neither, the composer
/// refuses and the action parks on an empty draft rather than pitching at
/// nothing. The target's display name is
/// read here because the evaluator's snapshot carries only its id.
async fn enrich_outreach_draft(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action: &mut AutopilotActionPayload,
) -> Result<(), RepositoryError> {
    use crowdrelay_domain::outreach::OutreachTargetKind;
    use crowdrelay_domain::outreach_letter::{compose_outreach_letter, OutreachLetterInput};

    let AutopilotActionPayload::RequestOutreach {
        draft,
        target_id,
        opportunity_id,
        phase,
        template_key,
        ..
    } = action
    else {
        return Ok(());
    };

    let ws = workspace_id.into_uuid();
    if template_key == "outreach.thread.v1" {
        return enrich_thread_draft(transaction, workspace_id, target_id, draft).await;
    }
    let target = sqlx::query_as::<_, (String, String, String)>(
        "SELECT display_name, target_kind, contact_email FROM outreach_targets WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws)
    .bind(target_id.into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    let Some((target_name, target_kind, contact_email)) = target else {
        // A target the action names must exist — the candidate carried its id.
        return Err(RepositoryError::NotFound);
    };
    let Some(kind) = OutreachTargetKind::parse(&target_kind) else {
        // A kind the vocabulary does not know is not a letter we can write.
        return Ok(());
    };
    // The same pitch the supply refresh keys opportunities by: a release
    // plan with a listen link, else the newest dated catalogue release.
    let pitch = crate::autopilot::outreach_supply::outreach_pitch(transaction, ws).await?;
    let (pitch_title, pitch_url) = pitch
        .map(|pitch| (pitch.title, pitch.url))
        .unwrap_or_default();
    // An organiser letter cites the act's next confirmed show as the reason
    // the ask is serious. Only that kind reads the field; the nearest
    // published show is a fact, and a calendar without one writes no line.
    let next_show = if kind == OutreachTargetKind::Organiser {
        sqlx::query_as::<_, (OffsetDateTime, Option<String>)>(
            "SELECT event.starts_at, city.name
             FROM events AS event
             LEFT JOIN cities AS city ON city.id = event.city_id
             WHERE event.workspace_id = $1
               AND event.status = 'published'
               AND event.starts_at >= now()
             ORDER BY event.starts_at
             LIMIT 1",
        )
        .bind(ws)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(map_sqlx)?
        .map(|(starts_at, city)| (starts_at.date(), city.unwrap_or_default()))
    } else {
        None
    };
    let sender = sender_identity_in_tx(transaction, ws).await?;
    // A show opportunity writes the show letter. Until 2026-09-27 it fell
    // through to the catalogue pitch below — "we would love to submit
    // {album} for coverage" — so every letter the event lifecycle queued
    // was an album review request that never named the show it was for.
    // An organiser is the exception: its letter asks for a slot and
    // already cites the calendar.
    if kind != OutreachTargetKind::Organiser
        && let Some(show) = show_for_opportunity(transaction, ws, opportunity_id.into_uuid()).await?
    {
        use crowdrelay_domain::show_pitch_letter::{ShowPitchInput, compose_show_pitch_letter};
        match compose_show_pitch_letter(&ShowPitchInput {
            sender: &sender,
            target_name: &target_name,
            target_kind: kind,
            phase: *phase,
            language: crowdrelay_domain::outreach_letter::language_for_contact(&contact_email),
            show_date: show.starts_at.date(),
            city: &show.city,
            venue: show.venue.as_deref(),
            ticket_url: show.ticket_url.as_deref(),
            listen_url: Some(pitch_url.as_str()),
        }) {
            Ok(letter) => {
                *draft = letter;
                *template_key = show.template_key;
            }
            // Leaving the draft empty is the refusal: the executor will not
            // send an empty letter, and the catalogue pitch is exactly the
            // wrong letter to fall back to.
            Err(refusal) => tracing::info!(
                opportunity_id = %opportunity_id.into_uuid(),
                refusal = refusal.message(),
                "show opportunity composed no letter"
            ),
        }
        return Ok(());
    }
    if let Ok(letter) = compose_outreach_letter(&OutreachLetterInput {
        sender: &sender,
        target_name: &target_name,
        target_kind: kind,
        pitch_title: &pitch_title,
        pitch_url: &pitch_url,
        phase: *phase,
        language: crowdrelay_domain::outreach_letter::language_for_contact(&contact_email),
        next_show,
    }) {
        *draft = letter;
    }
    Ok(())
}

/// Composes the reply scaffold onto a `RequestOutreachReply` payload while
/// the action is still being written — the same rule as the pitch letters.
///
/// The scaffold's shape comes from what the ledger honestly knows: the
/// interaction's disposition first, then the sheet's raw verdict — a
/// `NEGOTIATING` keeps terms open while a bare `GMAIL_REPLY` gets the
/// holding text, because neither row carries the words the person actually
/// wrote. The operator edits against the real thread; this only decides
/// which starting point they see.
async fn enrich_reply_draft(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action: &mut AutopilotActionPayload,
) -> Result<(), RepositoryError> {
    use crowdrelay_domain::outreach::{OutreachReplyDisposition, OutreachTargetKind};
    use crowdrelay_domain::reply_letter::{ReplyLetterInput, ReplyShape, compose_reply_letter};
    use crowdrelay_domain::reply_verdict_map::{ImportedVerdict, map_sheet_verdict};

    let AutopilotActionPayload::RequestOutreachReply {
        draft,
        target_id,
        reply_disposition,
        sheet_verdict,
        ..
    } = action
    else {
        return Ok(());
    };

    let ws = workspace_id.into_uuid();
    let target = sqlx::query_as::<_, (String, String)>(
        "SELECT display_name, target_kind FROM outreach_targets WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws)
    .bind(target_id.into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    let Some((target_name, target_kind)) = target else {
        // A target the action names must exist — the candidate carried its id.
        return Err(RepositoryError::NotFound);
    };
    let shape = if reply_disposition == OutreachReplyDisposition::Positive.as_str()
        || sheet_verdict.as_deref().map(map_sheet_verdict)
            == Some(ImportedVerdict::Terminal(OutreachReplyDisposition::Positive))
    {
        ReplyShape::Positive
    } else if sheet_verdict
        .as_deref()
        .is_some_and(|verdict| verdict.trim().eq_ignore_ascii_case("negotiating"))
    {
        ReplyShape::Negotiation
    } else {
        ReplyShape::Holding
    };
    // The reply threads onto the same pitch the conversation started on —
    // the most recent active release with a link, same rule the pitch uses.
    // Absent, the letter shortens rather than refusing.
    let pitch = sqlx::query_as::<_, (String, String)>(
        "SELECT title, listen_url FROM release_plans WHERE workspace_id = $1 AND active AND listen_url IS NOT NULL AND btrim(listen_url) <> '' AND btrim(title) <> '' ORDER BY release_at DESC LIMIT 1",
    )
    .bind(ws)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    let (pitch_title, pitch_url) = pitch.unwrap_or_default();
    let sender = sender_identity_in_tx(transaction, ws).await?;
    if let Ok(letter) = compose_reply_letter(&ReplyLetterInput {
        sender: &sender,
        target_name: &target_name,
        target_kind: OutreachTargetKind::parse(&target_kind),
        shape,
        pitch_title: &pitch_title,
        pitch_url: &pitch_url,
    }) {
        *draft = letter;
    }
    Ok(())
}

/// Composes the one nudge a hand-started thread gets onto a
/// `RequestOutreach` payload whose template is `outreach.thread.v1`.
///
/// What it may say is narrower than a pitch: the import kept the date of the
/// act's own message and nothing else, so the letter names that date and
/// does not pretend to remember what the thread was about. The language
/// comes from the contact's mailbox — a `.pl` address reads Polish.
async fn enrich_thread_draft(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    target_id: &crowdrelay_domain::OutreachTargetId,
    draft: &mut crowdrelay_domain::outreach_letter::OutreachLetter,
) -> Result<(), RepositoryError> {
    use crowdrelay_domain::outreach_letter::{ThreadFollowUpInput, compose_thread_followup_letter};

    let ws = workspace_id.into_uuid();
    let target = sqlx::query_as::<_, (String, String, Option<OffsetDateTime>)>(
        r#"
        SELECT target.display_name, target.contact_email,
               (SELECT max(message.occurred_at)
                FROM outreach_interactions AS message
                WHERE message.workspace_id = target.workspace_id
                  AND message.target_id = target.id
                  AND message.direction = 'outbound'
                  AND message.opportunity_id IS NULL) AS thread_started_at
        FROM outreach_targets AS target
        WHERE target.workspace_id = $1 AND target.id = $2
        "#,
    )
    .bind(ws)
    .bind(target_id.into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    let Some((target_name, contact_email, thread_started_at)) = target else {
        // A target the action names must exist — the candidate carried its id.
        return Err(RepositoryError::NotFound);
    };
    let Some(thread_started_at) = thread_started_at else {
        // A thread letter without a thread is a letter inventing a past —
        // leave the draft empty so dispatch fails closed on it.
        return Ok(());
    };
    let sender = sender_identity_in_tx(transaction, ws).await?;
    if let Ok(letter) = compose_thread_followup_letter(&ThreadFollowUpInput {
        sender: &sender,
        target_name: &target_name,
        contact_email: &contact_email,
        thread_started_at,
    }) {
        *draft = letter;
    }
    Ok(())
}

/// Composes the application letter onto an `ApplyLiveOpportunity` payload
/// while the action is still being written — the same rule as the booking
/// and outreach letters.
///
/// The organiser's name and the call's title come from the opportunity row
/// itself; the language comes from its travel band (`poland` writes Polish,
/// anything else writes English); the pitch is the tenant's most recent
/// release plan with a listen link, which only shortens the letter when
/// absent rather than refusing it.
async fn enrich_application_draft(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action: &mut AutopilotActionPayload,
) -> Result<(), RepositoryError> {
    use crowdrelay_domain::application_letter::{compose_application_letter, ApplicationLetterInput};
    use crowdrelay_domain::gig_letter::LetterLanguage;

    let AutopilotActionPayload::ApplyLiveOpportunity {
        draft,
        opportunity_id,
        opportunity_kind,
        ..
    } = action
    else {
        return Ok(());
    };

    let ws = workspace_id.into_uuid();
    // The letter's language is the organiser's country, not the travel-cost
    // band — `travel_band` says how far the van drives, `country_code` says
    // which language lands in the organiser's inbox.
    let opportunity = sqlx::query_as::<
        _,
        (String, String, Option<String>, Option<OffsetDateTime>),
    >(
        "SELECT title, organization, country_code, deadline FROM team_opportunities WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws)
    .bind(opportunity_id.into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    let Some((title, organization, country_code, deadline)) = opportunity else {
        // An opportunity the action names must exist — the candidate
        // carried its id.
        return Err(RepositoryError::NotFound);
    };
    let language = LetterLanguage::for_country(country_code.as_deref().unwrap_or(""));
    let pitch = sqlx::query_as::<_, (String, String)>(
        "SELECT title, listen_url FROM release_plans WHERE workspace_id = $1 AND active AND listen_url IS NOT NULL AND btrim(listen_url) <> '' AND btrim(title) <> '' ORDER BY release_at DESC LIMIT 1",
    )
    .bind(ws)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    let sender = sender_identity_in_tx(transaction, ws).await?;
    if let Ok(letter) = compose_application_letter(&ApplicationLetterInput {
        language,
        sender: &sender,
        opportunity_title: &title,
        organization: &organization,
        kind: *opportunity_kind,
        deadline,
        pitch_title: pitch.as_ref().map(|(t, _)| t.as_str()),
        pitch_url: pitch.as_ref().map(|(_, u)| u.as_str()),
    }) {
        *draft = letter;
    }
    Ok(())
}

/// Letters re-composed per call — a bound, not a policy: a queue this deep is
/// already a backlog nobody will read in one sitting.
const RECOMPOSE_BATCH: i64 = 200;

impl PostgresAutopilotRepository {
    /// Re-composes the letters still waiting on a person from the current
    /// sender settings and returns how many changed.
    ///
    /// A letter is composed when its action is written, so its sender line —
    /// the act's name, sound, home city and site — is frozen at that moment.
    /// A band that fixes "where the act is from" after the fact, or letters
    /// composed by the most-played-city heuristic before `act_home_city`
    /// existed, would otherwise keep proposing a letter that names the wrong
    /// city until each one expired. Re-running the same composer the write
    /// ran fixes the words and changes nothing else: recipients, targets and
    /// cost are the payload's facts and the enrichers never touch them.
    ///
    /// Skipped: pitches inside a sealed outreach wave — the batch review
    /// covered those exact words — drafts the band already edited, and reply
    /// scaffolds and threads, whose text does not carry the sender line.
    ///
    /// # Errors
    ///
    /// Database errors propagate.
    pub async fn recompose_pending_letters(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<u64, RepositoryError> {
        let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
        let rows = sqlx::query_as::<_, (Uuid, serde_json::Value)>(
            r#"
            SELECT action.id, action.payload
            FROM autopilot_actions AS action
            WHERE action.workspace_id = $1
              AND action.status = 'awaiting_approval'
              AND action.action_kind IN
                  ('booking.outreach.request', 'outreach.request', 'opportunity.live.apply')
              AND NOT EXISTS (
                  SELECT 1 FROM outreach_waves AS wave
                  WHERE wave.workspace_id = action.workspace_id
                    AND action.payload->>'wave_id' = wave.id::text
                    AND wave.state = 'sealed'
              )
              -- A draft the band already edited is theirs: re-composing it
              -- would throw their words away.
              AND NOT EXISTS (
                  SELECT 1 FROM draft_revisions AS revision
                  WHERE revision.workspace_id = action.workspace_id
                    AND revision.action_id = action.id
              )
            ORDER BY action.created_at
            LIMIT $2
            FOR UPDATE OF action SKIP LOCKED
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(RECOMPOSE_BATCH)
        .fetch_all(&mut *transaction)
        .await
        .map_err(map_sqlx)?;
        let mut changed = 0u64;
        for (id, stored) in rows {
            let Ok(mut action) = serde_json::from_value::<AutopilotActionPayload>(stored.clone())
            else {
                continue;
            };
            match &action {
                AutopilotActionPayload::RequestBookingOutreach { .. } => {
                    enrich_booking_draft(&mut transaction, workspace_id, &mut action).await?;
                }
                AutopilotActionPayload::RequestOutreach { template_key, .. }
                    if template_key != "outreach.thread.v1" =>
                {
                    enrich_outreach_draft(&mut transaction, workspace_id, &mut action).await?;
                }
                AutopilotActionPayload::ApplyLiveOpportunity { .. } => {
                    enrich_application_draft(&mut transaction, workspace_id, &mut action).await?;
                }
                _ => continue,
            }
            let recomposed = serde_json::to_value(&action).map_err(|_| RepositoryError::Unexpected)?;
            if recomposed == stored {
                continue;
            }
            sqlx::query(
                "UPDATE autopilot_actions SET payload = $3, updated_at = now() \
                 WHERE workspace_id = $1 AND id = $2 AND status = 'awaiting_approval'",
            )
            .bind(workspace_id.into_uuid())
            .bind(id)
            .bind(&recomposed)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            changed += 1;
        }
        transaction.commit().await.map_err(map_sqlx)?;
        Ok(changed)
    }
}

impl PostgresAutopilotRepository {
    /// Saves an operator's fix to a waiting draft's words without approving
    /// it, and returns the draft's revisable fields as they now read.
    ///
    /// Approve-with-edit is the usual door, but a pitch inside an outreach
    /// wave cannot be approved on its own — the wave is approved as a batch —
    /// so a wrong sentence in one letter had no way to be fixed before the
    /// batch went out. The same review gate applies (`draft_revision`: words
    /// only, never who reads them or what they point at), and the edit is
    /// recorded in `draft_revisions` like an approve-time edit, so the voice
    /// signal sees it and `recompose_pending_letters` leaves the letter alone.
    ///
    /// # Errors
    ///
    /// `NotFound` when no waiting action has this id; `ConflictBecause` with
    /// the review's reason when the revision is refused.
    pub async fn revise_pending_draft(
        &self,
        workspace_id: WorkspaceId,
        action_id: Uuid,
        revision: &std::collections::BTreeMap<String, String>,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<std::collections::BTreeMap<String, String>, RepositoryError> {
        let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
        // The audit row first — it is also the revision ledger's operation.
        // A replayed key answers with the draft as it reads now.
        let operation_id = Uuid::now_v7();
        let replay = operator_actions::insert_operator_action(
            &mut transaction,
            workspace_id,
            operation_id,
            "revise_autopilot_action_draft",
            "autopilot_action",
            action_id,
            "admin_api_key",
            idempotency_key,
            request_id,
            &serde_json::json!({ "fields": revision.keys().collect::<Vec<_>>() }),
        )
        .await?;
        if replay.is_some() {
            let payload = sqlx::query_scalar::<_, serde_json::Value>(
                "SELECT payload FROM autopilot_actions WHERE workspace_id = $1 AND id = $2",
            )
            .bind(workspace_id.into_uuid())
            .bind(action_id)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?
            .ok_or(RepositoryError::NotFound)?;
            transaction.commit().await.map_err(map_sqlx)?;
            return Ok(crowdrelay_domain::draft_revision::revisable_fields(&payload));
        }
        let Some(mut payload) = sqlx::query_scalar::<_, serde_json::Value>(
            "SELECT payload FROM autopilot_actions \
             WHERE workspace_id = $1 AND id = $2 AND status = 'awaiting_approval' \
             FOR UPDATE",
        )
        .bind(workspace_id.into_uuid())
        .bind(action_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(map_sqlx)?
        else {
            return Err(RepositoryError::NotFound);
        };
        let draft = crowdrelay_domain::draft_revision::revisable_fields(&payload);
        let changed = crowdrelay_domain::draft_revision::review_revision(&draft, revision)
            .map_err(|refusal| RepositoryError::ConflictBecause(refusal.conflict_reason()))?;
        crowdrelay_domain::draft_revision::apply_revision(&mut payload, &changed);
        sqlx::query(
            "UPDATE autopilot_actions SET payload = $3, updated_at = now() \
             WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace_id.into_uuid())
        .bind(action_id)
        .bind(&payload)
        .execute(&mut *transaction)
        .await
        .map_err(map_sqlx)?;
        for (field, after) in &changed {
            let mut single = std::collections::BTreeMap::new();
            single.insert(field.clone(), after.clone());
            let distance = crowdrelay_domain::draft_revision::revision_distance(&draft, &single);
            // A second save of the same field keeps the machine's words as the
            // `before` — the distance is how far the band moved the machine's
            // draft, not the band's own previous edit.
            sqlx::query(
                r#"
                INSERT INTO draft_revisions
                    (workspace_id, action_id, operation_id, field,
                     before_text, after_text, distance_chars)
                VALUES ($1, $2, $3, $4, $5, $6, $7)
                ON CONFLICT (workspace_id, action_id, field) DO UPDATE SET
                    operation_id = EXCLUDED.operation_id,
                    after_text = EXCLUDED.after_text,
                    distance_chars = EXCLUDED.distance_chars
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(action_id)
            .bind(operation_id)
            .bind(field)
            .bind(draft.get(field).cloned().unwrap_or_default())
            .bind(after)
            .bind(i64::try_from(distance).unwrap_or(i64::MAX))
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
        }
        transaction.commit().await.map_err(map_sqlx)?;
        Ok(crowdrelay_domain::draft_revision::revisable_fields(&payload))
    }
}

/// The show a show opportunity is about, with what its letter cites.
struct OpportunityShow {
    starts_at: OffsetDateTime,
    city: String,
    venue: Option<String>,
    ticket_url: Option<String>,
    template_key: String,
}

/// `Some` when the opportunity is an `event` opportunity whose show still
/// stands; `None` for every other subject, which keeps its own letter.
async fn show_for_opportunity(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ws: Uuid,
    opportunity_id: Uuid,
) -> Result<Option<OpportunityShow>, RepositoryError> {
    let row = sqlx::query_as::<_, (OffsetDateTime, Option<String>, Option<String>, Option<String>, String)>(
        r#"
        SELECT event.starts_at, city.name, event.venue, event.ticket_url,
               opportunity.template_key
        FROM outreach_opportunities AS opportunity
        JOIN events AS event
          ON event.workspace_id = opportunity.workspace_id
         AND 'event:' || event.id::text = opportunity.subject_key
        LEFT JOIN cities AS city ON city.id = event.city_id
        WHERE opportunity.workspace_id = $1
          AND opportunity.id = $2
          AND opportunity.subject_kind = 'event'
        "#,
    )
    .bind(ws)
    .bind(opportunity_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    Ok(row.map(|(starts_at, city, venue, ticket_url, template_key)| OpportunityShow {
        starts_at,
        city: city.unwrap_or_default(),
        venue,
        ticket_url,
        template_key,
    }))
}
