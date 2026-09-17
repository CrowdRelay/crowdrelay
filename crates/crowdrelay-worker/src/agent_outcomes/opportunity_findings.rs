/// Parses a scout finding's `observed_at` into a timestamp.
///
/// `observed_at` carries the source's own date when the source states one —
/// a dated listing, a dated post — and when the item omits it the outcome's
/// write time stands in instead. `None` for an absent or empty field;
/// rejects unparseable values and dates more than a day in the future: a
/// source cannot be dated tomorrow.
fn parse_finding_timestamp(
    value: Option<&Value>,
) -> Result<Option<OffsetDateTime>, OutcomeRejection> {
    let Some(text) = value.and_then(Value::as_str).map(str::trim) else {
        return Ok(None);
    };
    if text.is_empty() {
        return Ok(None);
    }
    let parsed = OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
        .map_err(|_| OutcomeRejection::InvalidFindingTimestamp)?;
    if parsed > OffsetDateTime::now_utc() + Duration::from_secs(86_400) {
        return Err(OutcomeRejection::InvalidFindingTimestamp);
    }
    Ok(Some(parsed))
}

/// Parses an optional finding date — `YYYY-MM-DD` or RFC3339, the two shapes
/// the agents contract writes. `None` when absent or unreadable: an optional
/// fact the model got wrong drops rather than sinking the whole finding.
fn parse_finding_date(value: Option<&Value>) -> Option<OffsetDateTime> {
    let text = value?.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(parsed) = OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
    {
        return Some(parsed);
    }
    time::Date::parse(text, &time::format_description::well_known::Iso8601::DATE)
        .ok()
        .map(|date| date.midnight().assume_utc())
}

/// A contact email the model claims to have seen on the source. Accepted only
/// when it fits the column's own shape; a malformed one drops rather than
/// failing the row's CHECK constraint and sinking the finding.
fn plausible_finding_email(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?.trim();
    (text.len() <= 320
        && text.contains('@')
        && text.contains('.')
        && !text.chars().any(char::is_whitespace))
    .then(|| text.to_owned())
}

/// A country code is `^[A-Z]{2}$` on the column; anything else drops.
fn finding_country_code(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?.trim().to_ascii_uppercase();
    (text.len() == 2 && text.bytes().all(|byte| byte.is_ascii_uppercase())).then_some(text)
}

    /// status, eligibility, verification, strategic value, and any metadata
/// keys outside the scout's own `discovery` block.
///
/// Nothing the model claims is trusted: money columns stay zero, the
/// destination stays unverified, the contract flag stays set so no
/// finding can reach auto-submission, and dates or an address the model
/// produced drop rather than failing the row's CHECK constraints.
async fn insert_opportunity_finding(
    tx: &mut Transaction<'_, Postgres>,
    outcome: &ValidatedOutcome,
    item: &Value,
) -> Result<Uuid, AgentOutcomeError> {
    let title = item
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let summary = item
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let destination_url = item
        .get("destination_url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let declared_kind = item
        .get("opportunity_kind")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if title.is_empty() || summary.is_empty() {
        return Err(OutcomeRejection::MissingFindingContent.into());
    }
    if destination_url.is_empty() || destination_url.len() > 1_000 {
        return Err(OutcomeRejection::MissingFindingLink.into());
    }
    let Some(declared) =
        crowdrelay_domain::live_opportunities::ScoutOpportunityKind::parse(declared_kind)
    else {
        return Err(OutcomeRejection::InvalidFindingKind {
            kind: declared_kind.to_owned(),
        }
        .into());
    };

    // The assessor wins when the finding's own text classifies — the same
    // source of truth the HTTP discovery endpoint consults. The model's
    // declared kind is a fallback for text the keyword matcher cannot
    // see through, scored at the floor because the text could not support
    // its own claim.
    let discovery =
        crowdrelay_domain::live_opportunities::LiveOpportunityDiscovery { title, summary };
    let (kind, fit, reputation, confidence_basis_points) =
        match crowdrelay_domain::live_opportunities::evaluate_scout_discovery(&discovery) {
            Some(assessment) => (
                assessment.kind,
                assessment.fit_basis_points,
                assessment.reputation_basis_points,
                assessment.confidence.basis_points(),
            ),
            None => (declared, 3_000, 5_000, 3_000),
        };

    // `agent_scout` is the finding's source: the model named the finding,
    // not a market feed. It satisfies `valid_market_source`
    // ([A-Za-z0-9_.:-]+, <=64) so a later API upsert of the same row is
    // symmetric.
    let source = "agent_scout";
    let external_key = {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        hasher.update(kind.as_str().as_bytes());
        hasher.update(b"|");
        hasher.update(destination_url.as_bytes());
        format!("scout:{}", hex::encode(hasher.finalize()))
    };
    // `organization` is NOT NULL non-blank; a finding that cannot name
    // who runs the thing still lands under the scout's name so the
    // review row is honest about what is known.
    let organization = item
        .get("organization")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 240)
        .unwrap_or(source);

    // The source's own date when the model supplies a readable one —
    // stale sources land stale. Otherwise the outcome's write time is
    // the honest bound: the finding cannot have been observed after the
    // run that reported it finished.
    let observed_at = match parse_finding_timestamp(item.get("observed_at"))? {
        Some(at) => at,
        None => {
            sqlx::query_scalar::<_, OffsetDateTime>(
                "SELECT created_at FROM agent_outcomes WHERE id = $1 AND workspace_id = $2",
            )
            .bind(outcome.id)
            .bind(outcome.workspace_id)
            .fetch_one(&mut **tx)
            .await?
        }
    };

    let opportunity_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO viryaos_team_opportunities
            (workspace_id, opportunity_kind, source, external_key, title,
             organization, destination_url, contact_email, country_code,
             verified_destination, fit_basis_points, reputation_basis_points,
             confidence_basis_points, currency, expected_fee_minor,
             estimated_cost_minor, application_fee_minor, requires_contract,
             exclusive, eligible, deadline, event_starts_at,
             strategic_value_basis_points, status, metadata, source_observed_at)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,false,$10,$11,$12,'PLN',
                0,0,0,true,false,true,$13,$14,0,'new',$15,$16)
        ON CONFLICT (workspace_id, source, external_key) DO UPDATE SET
            title = EXCLUDED.title,
            organization = EXCLUDED.organization,
            contact_email = COALESCE(
                EXCLUDED.contact_email, viryaos_team_opportunities.contact_email),
            country_code = COALESCE(
                EXCLUDED.country_code, viryaos_team_opportunities.country_code),
            deadline = COALESCE(EXCLUDED.deadline, viryaos_team_opportunities.deadline),
            event_starts_at = COALESCE(
                EXCLUDED.event_starts_at, viryaos_team_opportunities.event_starts_at),
            fit_basis_points = GREATEST(
                EXCLUDED.fit_basis_points, viryaos_team_opportunities.fit_basis_points),
            reputation_basis_points = GREATEST(
                EXCLUDED.reputation_basis_points,
                viryaos_team_opportunities.reputation_basis_points),
            confidence_basis_points = GREATEST(
                EXCLUDED.confidence_basis_points,
                viryaos_team_opportunities.confidence_basis_points),
            -- The scout owns only its `discovery` block; every other key
            -- the operator or the loop wrote is preserved.
            metadata = viryaos_team_opportunities.metadata || EXCLUDED.metadata,
            -- The most recent observation stands.
            source_observed_at = GREATEST(
                COALESCE(viryaos_team_opportunities.source_observed_at,
                         EXCLUDED.source_observed_at),
                COALESCE(EXCLUDED.source_observed_at,
                         viryaos_team_opportunities.source_observed_at)),
            updated_at = now(),
            version = viryaos_team_opportunities.version + 1
        RETURNING id
        "#,
    )
    .bind(outcome.workspace_id)
    .bind(kind.as_str())
    .bind(source)
    .bind(external_key)
    .bind(title)
    .bind(organization)
    .bind(destination_url)
    .bind(plausible_finding_email(item.get("contact_email")))
    .bind(finding_country_code(item.get("country_code")))
    .bind(i32::from(fit))
    .bind(i32::from(reputation))
    .bind(i32::from(confidence_basis_points))
    .bind(parse_finding_date(item.get("deadline")))
    .bind(parse_finding_date(item.get("event_starts_at")))
    .bind(json!({
        "discovery": {
            "destination_unverified": true,
            "fee_unverified": true,
            "terms_unverified": true,
            "summary": summary,
            "outcome_id": outcome.id,
            "task_id": outcome.task_id,
            "declared_kind": declared.as_str(),
        }
    }))
    .bind(Some(observed_at))
    .fetch_one(&mut **tx)
    .await?;
    Ok(opportunity_id)
}

#[cfg(test)]
mod finding_tests {
    use super::*;
    use super::tests::make_outcome;
    use crowdrelay_application::agent_outcomes::OutcomeKind;

#[test]
fn finding_timestamp_absent_means_no_source_date() {
    assert_eq!(parse_finding_timestamp(None), Ok(None));
    assert_eq!(parse_finding_timestamp(Some(&json!(""))), Ok(None));
    assert_eq!(parse_finding_timestamp(Some(&json!(null))), Ok(None));
}

#[test]
fn finding_timestamp_accepts_a_readable_past_date() {
    let parsed = parse_finding_timestamp(Some(&json!("2026-01-15T10:00:00Z")));
    assert!(matches!(parsed, Ok(Some(_))));
}

#[test]
fn finding_timestamp_rejects_unreadable_and_future_dates() {
    assert_eq!(
        parse_finding_timestamp(Some(&json!("not a date"))),
        Err(OutcomeRejection::InvalidFindingTimestamp)
    );
    let future = (OffsetDateTime::now_utc() + Duration::from_secs(3 * 86_400))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    assert_eq!(
        parse_finding_timestamp(Some(&json!(future))),
        Err(OutcomeRejection::InvalidFindingTimestamp)
    );
}

#[test]
fn finding_date_accepts_date_only_and_rfc3339() {
    assert!(parse_finding_date(Some(&json!("2026-06-01"))).is_some());
    assert!(parse_finding_date(Some(&json!("2026-06-01T00:00:00Z"))).is_some());
    assert_eq!(parse_finding_date(Some(&json!("June"))), None);
    assert_eq!(parse_finding_date(None), None);
}

#[test]
fn finding_email_requires_an_email_shape() {
    assert_eq!(
        plausible_finding_email(Some(&json!("editor@zine.example"))).as_deref(),
        Some("editor@zine.example")
    );
    assert_eq!(plausible_finding_email(Some(&json!("no at sign"))), None);
    assert_eq!(plausible_finding_email(Some(&json!("a@b"))), None);
    assert_eq!(plausible_finding_email(None), None);
}

#[test]
fn finding_country_code_is_two_ascii_letters() {
    assert_eq!(
        finding_country_code(Some(&json!("pl"))).as_deref(),
        Some("PL")
    );
    assert_eq!(finding_country_code(Some(&json!("POL"))), None);
    assert_eq!(finding_country_code(None), None);
}

#[test]
fn a_finding_without_a_link_fails_the_guard() {
    let outcome = make_outcome(
        OutcomeKind::OpportunityFindings,
        8_000,
        Some(json!({
            "opportunity_kind": "press",
            "title": "Unsigned column",
            "summary": "A quarterly zine column.",
        })),
    );
    assert_eq!(
        evaluate_outcome_quality(&outcome),
        Err(OutcomeRejection::MissingFindingLink)
    );
}

#[test]
fn a_finding_outside_the_vocabulary_fails_the_guard() {
    let outcome = make_outcome(
        OutcomeKind::OpportunityFindings,
        8_000,
        Some(json!({
            "opportunity_kind": "funding",
            "title": "A grant",
            "summary": "A regional music fund.",
            "destination_url": "https://fund.example/apply",
        })),
    );
    assert_eq!(
        evaluate_outcome_quality(&outcome),
        Err(OutcomeRejection::InvalidFindingKind {
            kind: "funding".to_owned()
        })
    );
}

#[test]
fn a_valid_finding_passes_the_guard() {
    let outcome = make_outcome(
        OutcomeKind::OpportunityFindings,
        8_000,
        Some(json!({
            "opportunity_kind": "press",
            "title": "Unsigned column",
            "summary": "A quarterly zine column.",
            "destination_url": "https://zine.example/columns/unsigned",
        })),
    );
    evaluate_outcome_quality(&outcome).expect("a linked, in-vocabulary finding must pass");
}
}
