//! Event-network scout outcome -> reviewed Beacon graph candidate.
//!
//! The model discovers; it never authorises contact. Every row lands active
//! but unverified and non-contactable, exactly like the existing network
//! discovery intake. The operator/verification lifecycle decides whether a
//! candidate becomes a relationship.

async fn insert_beacon_candidate(
    tx: &mut Transaction<'_, Postgres>,
    outcome: &ValidatedOutcome,
    item: &Value,
    producing_prompt: Option<&str>,
) -> Result<Uuid, AgentOutcomeError> {
    let Some(prompt) = producing_prompt else {
        return Err(OutcomeRejection::UngroundedBeaconCandidate {
            reason: "producing task prompt is unavailable".to_owned(),
        }
        .into());
    };

    let event_id = item
        .get("event_id")
        .and_then(Value::as_str)
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .ok_or_else(|| OutcomeRejection::UngroundedBeaconCandidate {
            reason: "event_id is missing or invalid".to_owned(),
        })?;
    if !prompt.contains(&event_id.to_string()) {
        return Err(OutcomeRejection::UngroundedBeaconCandidate {
            reason: "event_id was not in the Brain's research brief".to_owned(),
        }
        .into());
    }

    let kind = item
        .get("beacon_kind")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if !matches!(
        kind,
        "radio"
            | "local_press"
            | "television"
            | "reviewer"
            | "creator"
            | "photographer"
            | "promoter"
            | "venue"
            | "scene_partner"
            | "patron"
            | "community"
    ) {
        return Err(OutcomeRejection::UngroundedBeaconCandidate {
            reason: format!("unsupported beacon_kind {kind:?}"),
        }
        .into());
    }

    let display_name = item
        .get("display_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if display_name.is_empty() || display_name.chars().count() > 240 {
        return Err(OutcomeRejection::UngroundedBeaconCandidate {
            reason: "display_name is missing or too long".to_owned(),
        }
        .into());
    }

    let source_url = item
        .get("source_url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if !source_url.starts_with("https://") || source_url.len() > 2048 || !prompt.contains(source_url)
    {
        return Err(OutcomeRejection::UngroundedBeaconCandidate {
            reason: "source_url is not an https URL present in the research context".to_owned(),
        }
        .into());
    }

    let contact_email = item
        .get("contact_email")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_lowercase);
    if let Some(email) = contact_email.as_deref()
        && (email.len() > 320 || !email.contains('@') || !prompt.contains(email))
    {
        return Err(OutcomeRejection::UngroundedBeaconCandidate {
            reason: "contact_email was not present in the research context".to_owned(),
        }
        .into());
    }

    let explicit_destination = item
        .get("destination_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(destination) = explicit_destination
        && (!destination.starts_with("https://")
            || destination.len() > 2048
            || !prompt.contains(destination))
    {
        return Err(OutcomeRejection::UngroundedBeaconCandidate {
            reason: "destination_url is not an https URL present in the research context".to_owned(),
        }
        .into());
    }
    // A public evidence/profile page is also a reviewable destination when
    // the source did not expose a separate contact route.
    let destination_url = explicit_destination.unwrap_or(source_url);

    let city_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT city_id
        FROM events
        WHERE workspace_id=$1 AND id=$2
        "#,
    )
    .bind(outcome.workspace_id)
    .bind(event_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| OutcomeRejection::UngroundedBeaconCandidate {
        reason: "event_id does not belong to this workspace".to_owned(),
    })?;

    let metadata = json!({
        "event_network_scout": {
            "event_id": event_id,
            "agent_outcome_id": outcome.id,
            "source_url": source_url,
            "why_fit": item.get("why_fit").and_then(Value::as_str),
            "human_review_required": true,
            "marketing_email_consent_confirmed": false,
        }
    });

    // Identity policy mirrors the canonical Beacon upsert: email when known,
    // otherwise destination URL, scoped by city + kind.
    let existing = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT id
        FROM beacons
        WHERE workspace_id=$1 AND city_id=$2 AND beacon_kind=$3
          AND (
            ($4::text IS NOT NULL AND contact_email=$4)
            OR ($4::text IS NULL AND contact_email IS NULL AND destination_url=$5)
          )
        ORDER BY id
        LIMIT 1
        FOR UPDATE
        "#,
    )
    .bind(outcome.workspace_id)
    .bind(city_id)
    .bind(kind)
    .bind(contact_email.as_deref())
    .bind(destination_url)
    .fetch_optional(&mut **tx)
    .await?;

    if let Some(beacon_id) = existing {
        sqlx::query(
            r#"
            UPDATE beacons
            SET source_url=COALESCE(source_url,$3),
                metadata=metadata || $4::jsonb,
                version=version+1
            WHERE workspace_id=$1 AND id=$2
            "#,
        )
        .bind(outcome.workspace_id)
        .bind(beacon_id)
        .bind(source_url)
        .bind(&metadata)
        .execute(&mut **tx)
        .await?;
        return Ok(beacon_id);
    }

    let beacon_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO beacons (
            id,workspace_id,city_id,beacon_kind,display_name,contact_email,
            destination_url,source_url,active,verified,accepts_outreach,
            do_not_contact,relationship_score,relevance_basis_points,
            confidence_basis_points,metadata
        ) VALUES (
            $1,$2,$3,$4,$5,$6,$7,$8,
            true,false,false,false,
            0,5000,1,$9
        )
        ON CONFLICT DO NOTHING
        "#,
    )
    .bind(beacon_id)
    .bind(outcome.workspace_id)
    .bind(city_id)
    .bind(kind)
    .bind(display_name)
    .bind(contact_email.as_deref())
    .bind(destination_url)
    .bind(source_url)
    .bind(&metadata)
    .execute(&mut **tx)
    .await?;

    // A concurrent scout may have won the natural-key race. Resolve the
    // canonical row rather than inventing a second candidate.
    let canonical = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT id
        FROM beacons
        WHERE workspace_id=$1 AND city_id=$2 AND beacon_kind=$3
          AND (
            ($4::text IS NOT NULL AND contact_email=$4)
            OR ($4::text IS NULL AND contact_email IS NULL AND destination_url=$5)
          )
        ORDER BY id
        LIMIT 1
        "#,
    )
    .bind(outcome.workspace_id)
    .bind(city_id)
    .bind(kind)
    .bind(contact_email.as_deref())
    .bind(destination_url)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AgentOutcomeError::UnpersistablePayload)?;

    Ok(canonical)
}
