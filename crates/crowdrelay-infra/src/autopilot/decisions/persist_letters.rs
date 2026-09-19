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
/// act has played most and its own site. Shared by the booking and outreach
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
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty()),
        home_city: sqlx::query_scalar::<_, String>(
            r#"
            SELECT city.name
            FROM events AS event
            JOIN cities AS city ON city.id = event.city_id
            WHERE event.workspace_id = $1
            GROUP BY city.id, city.name
            ORDER BY count(*) DESC, city.name
            LIMIT 1
            "#,
        )
        .bind(workspace_id)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(map_sqlx)?,
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
/// listen link; with none, the composer refuses and the action parks on an
/// empty draft rather than pitching at nothing. The target's display name is
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
        phase,
        ..
    } = action
    else {
        return Ok(());
    };

    let ws = workspace_id.into_uuid();
    let target = sqlx::query_as::<_, (String, String)>(
        "SELECT display_name, target_kind FROM viryaos_outreach_targets WHERE workspace_id = $1 AND id = $2",
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
    let Some(kind) = OutreachTargetKind::parse(&target_kind) else {
        // A kind the vocabulary does not know is not a letter we can write.
        return Ok(());
    };
    let pitch = sqlx::query_as::<_, (String, String)>(
        "SELECT title, listen_url FROM viryaos_release_plans WHERE workspace_id = $1 AND active AND listen_url IS NOT NULL AND btrim(listen_url) <> '' AND btrim(title) <> '' ORDER BY release_at DESC LIMIT 1",
    )
    .bind(ws)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    let (pitch_title, pitch_url) = pitch.unwrap_or_default();
    let sender = sender_identity_in_tx(transaction, ws).await?;
    if let Ok(letter) = compose_outreach_letter(&OutreachLetterInput {
        sender: &sender,
        target_name: &target_name,
        target_kind: kind,
        pitch_title: &pitch_title,
        pitch_url: &pitch_url,
        phase: *phase,
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
        "SELECT title, organization, country_code, deadline FROM viryaos_team_opportunities WHERE workspace_id = $1 AND id = $2",
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
        "SELECT title, listen_url FROM viryaos_release_plans WHERE workspace_id = $1 AND active AND listen_url IS NOT NULL AND btrim(listen_url) <> '' AND btrim(title) <> '' ORDER BY release_at DESC LIMIT 1",
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
