// Event-network scout outcome -> reviewed Beacon graph candidate.
//
// The model discovers; it never authorises contact. Every row lands active
// but unverified and non-contactable, exactly like the existing network
// discovery intake. The operator/verification lifecycle decides whether a
// candidate becomes a relationship.
//
// Grounding is checked against the task's recorded evidence — the bounded
// set of URLs and contacts the context builder actually rendered into the
// model's prompt — never against the original brief's text. Research
// results exist only inside that context, so a URL the brief could not
// have known is valid evidence and a URL the model invented is not.

/// URL/contacts the model was shown, normalised so the candidate's citation
/// and the stored evidence compare equal when they name the same thing.
fn normalize_evidence_url(raw: &str) -> String {
    let trimmed = raw.trim();
    let no_fragment = trimmed.split('#').next().unwrap_or_default();
    let no_trailing_slash = no_fragment.trim_end_matches('/');
    // Lowercase scheme and authority, keep the path byte-for-byte — identical
    // to normalizeEvidenceUrl on the agents side.
    match no_trailing_slash.split_once("://") {
        Some((scheme, rest)) => match rest.split_once('/') {
            Some((authority, path)) => format!(
                "{}://{}/{}",
                scheme.to_ascii_lowercase(),
                authority.to_ascii_lowercase(),
                path
            ),
            None => format!(
                "{}://{}",
                scheme.to_ascii_lowercase(),
                rest.to_ascii_lowercase()
            ),
        },
        None => no_trailing_slash.to_ascii_lowercase(),
    }
}

fn normalize_evidence_contact(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

fn evidence_urls(task_metadata: &Value) -> std::collections::HashMap<String, Value> {
    task_metadata
        .get("evidence")
        .and_then(|evidence| evidence.get("urls"))
        .and_then(Value::as_array)
        .map(|urls| {
            urls.iter()
                .filter_map(|entry| {
                    let raw = entry.get("url").and_then(Value::as_str)?;
                    Some((normalize_evidence_url(raw), entry.clone()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn evidence_contacts(task_metadata: &Value) -> std::collections::HashSet<String> {
    task_metadata
        .get("evidence")
        .and_then(|evidence| evidence.get("contacts"))
        .and_then(Value::as_array)
        .map(|contacts| {
            contacts
                .iter()
                .filter_map(|entry| entry.get("value"))
                .filter_map(Value::as_str)
                .map(normalize_evidence_contact)
                .collect()
        })
        .unwrap_or_default()
}

/// The decision's subject pair for a beacon-candidate outcome: the new beacon
/// when the outcome carries an item, the outcome itself when it does not (the
/// quality guard refuses an itemless candidate before the transaction opens;
/// the second arm only keeps the pair total).
async fn beacon_candidate_subject(
    tx: &mut Transaction<'_, Postgres>,
    outcome: &ValidatedOutcome,
    producing_task: Option<&(String, String, Value)>,
) -> Result<(&'static str, Uuid), AgentOutcomeError> {
    match &outcome.payload.item {
        Some(item) => Ok((
            "beacon",
            insert_beacon_candidate(tx, outcome, item, producing_task).await?,
        )),
        None => Ok(("agent_outcome", outcome.id)),
    }
}

async fn insert_beacon_candidate(
    tx: &mut Transaction<'_, Postgres>,
    outcome: &ValidatedOutcome,
    item: &Value,
    producing_task: Option<&(String, String, Value)>,
) -> Result<Uuid, AgentOutcomeError> {
    let Some((producing_template, _, task_metadata)) = producing_task else {
        return Err(OutcomeRejection::UngroundedBeaconCandidate {
            reason: "producing task is unavailable".to_owned(),
        }
        .into());
    };
    if producing_template != "event-network-scout" {
        return Err(OutcomeRejection::UngroundedBeaconCandidate {
            reason: format!(
                "beacon_candidates may only come from event-network-scout, got {producing_template:?}"
            ),
        }
        .into());
    }

    // The task pins exactly one event in its metadata at dispatch time. A
    // candidate for any other event — or for an event merely named in the
    // prompt — is a different show's research and is rejected.
    let event_id = item
        .get("event_id")
        .and_then(Value::as_str)
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .ok_or_else(|| OutcomeRejection::UngroundedBeaconCandidate {
            reason: "event_id is missing or invalid".to_owned(),
        })?;
    let pinned_event = task_metadata
        .get("subject_event_id")
        .and_then(Value::as_str)
        .and_then(|raw| Uuid::parse_str(raw).ok());
    if pinned_event != Some(event_id) {
        return Err(OutcomeRejection::UngroundedBeaconCandidate {
            reason: "event_id is not the event this task was pinned to".to_owned(),
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

    let urls = evidence_urls(task_metadata);
    let contacts = evidence_contacts(task_metadata);

    let source_url = item
        .get("source_url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let source_evidence = (!source_url.is_empty())
        .then(|| urls.get(&normalize_evidence_url(source_url)))
        .flatten()
        .cloned();
    if !source_url.starts_with("https://") || source_url.len() > 2048 || source_evidence.is_none() {
        return Err(OutcomeRejection::UngroundedBeaconCandidate {
            reason: "source_url is not among the URLs this task's evidence showed the model"
                .to_owned(),
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
        && (email.len() > 320 || !email.contains('@') || !contacts.contains(&normalize_evidence_contact(email)))
    {
        return Err(OutcomeRejection::UngroundedBeaconCandidate {
            reason: "contact_email was not present in the task's recorded evidence".to_owned(),
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
            || !urls.contains_key(&normalize_evidence_url(destination)))
    {
        return Err(OutcomeRejection::UngroundedBeaconCandidate {
            reason: "destination_url is not among the URLs this task's evidence showed the model"
                .to_owned(),
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

    // Latest match wins inside the block; the full per-event history lives
    // in beacon_event_matches so the second show does not overwrite what
    // the first one taught us. Operator stamps (`network_review`) sit in a
    // sibling key and are never touched here.
    let metadata = json!({
        "event_network_scout": {
            "event_id": event_id,
            "agent_outcome_id": outcome.id,
            "source_url": source_url,
            "why_fit": item.get("why_fit").and_then(Value::as_str),
            // The proof an operator reviews: the snippet the model actually
            // saw, and which tool fetched it. Stored on the row itself — the
            // review queue must not need to walk outcome→task→metadata joins.
            "evidence": {
                "snippet": source_evidence.as_ref().and_then(|e| e.get("snippet")),
                "tool": source_evidence.as_ref().and_then(|e| e.get("tool")),
                "fetched_at": source_evidence.as_ref().and_then(|e| e.get("fetched_at")),
            },
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

    let beacon_id = if let Some(beacon_id) = existing {
        sqlx::query(
            r#"
            UPDATE beacons
            SET source_url=COALESCE(source_url,$3),
                metadata = metadata || jsonb_build_object(
                    'event_network_scout',
                    COALESCE(metadata->'event_network_scout','{}'::jsonb) || $4::jsonb
                ),
                version=version+1
            WHERE workspace_id=$1 AND id=$2
            "#,
        )
        .bind(outcome.workspace_id)
        .bind(beacon_id)
        .bind(source_url)
        .bind(metadata.get("event_network_scout").cloned().unwrap_or(Value::Null))
        .execute(&mut **tx)
        .await?;
        beacon_id
    } else {
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
        sqlx::query_scalar::<_, Uuid>(
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
        .ok_or(AgentOutcomeError::UnpersistablePayload)?
    };

    // Every accepted candidate — new beacon or repeat match — records the
    // (beacon, event) pair with the outcome that produced it. A repeat
    // match refreshes matched_at and matched_count; outcome_id stays the
    // FIRST outcome that made the match so the evidence trail points at
    // the original discovery, not the latest re-confirmation.
    sqlx::query(
        r#"
        INSERT INTO beacon_event_matches (workspace_id, beacon_id, event_id, outcome_id)
        VALUES ($1,$2,$3,$4)
        ON CONFLICT (workspace_id, beacon_id, event_id)
        DO UPDATE SET matched_at=now(), matched_count=beacon_event_matches.matched_count+1
        "#,
    )
    .bind(outcome.workspace_id)
    .bind(beacon_id)
    .bind(event_id)
    .bind(outcome.id)
    .execute(&mut **tx)
    .await?;

    Ok(beacon_id)
}
