// Grounded relationship research ingestion.
//
// The agent may only add context to the one Beacon the Brain pinned in task
// metadata, and only from a URL present in the bounded evidence set recorded by
// the agent service. The resulting PersonalHook then passes the same domain
// constructor as an operator-authored note before it enters the shared
// contact_research store.

fn parse_contact_research_day(raw: &str) -> Option<time::Date> {
    let mut parts = raw.trim().splitn(3, '-');
    let year = parts.next()?.parse::<i32>().ok()?;
    let month = time::Month::try_from(parts.next()?.parse::<u8>().ok()?).ok()?;
    let day = parts.next()?.parse::<u8>().ok()?;
    time::Date::from_calendar_date(year, month, day).ok()
}

fn contact_research_beacon_id(outcome: &ValidatedOutcome) -> Result<Uuid, OutcomeRejection> {
    outcome
        .payload
        .item
        .as_ref()
        .and_then(|item| item.get("beacon_id"))
        .and_then(Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok())
        .ok_or_else(|| OutcomeRejection::UngroundedContactResearch {
            reason: "item has no valid beacon_id".to_owned(),
        })
}

fn validate_contact_research_grounding(
    outcome: &ValidatedOutcome,
    producing_task: Option<&(String, String, Value)>,
) -> Result<(Uuid, crowdrelay_domain::contact_research::PersonalHook, String), OutcomeRejection> {
    let item = outcome
        .payload
        .item
        .as_ref()
        .ok_or_else(|| OutcomeRejection::UngroundedContactResearch {
            reason: "research outcome carries no item".to_owned(),
        })?;
    let beacon_id = contact_research_beacon_id(outcome)?;
    let Some((template, _, metadata)) = producing_task else {
        return Err(OutcomeRejection::UngroundedContactResearch {
            reason: "producing task metadata is unavailable".to_owned(),
        });
    };
    if template != "contact-research" {
        return Err(OutcomeRejection::UngroundedContactResearch {
            reason: format!("producing template {template:?} is not contact-research"),
        });
    }
    let pinned = metadata
        .get("subject_beacon_id")
        .and_then(Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok());
    if pinned != Some(beacon_id) {
        return Err(OutcomeRejection::UngroundedContactResearch {
            reason: format!(
                "item beacon_id {beacon_id} does not match the Brain-pinned subject {:?}",
                pinned
            ),
        });
    }

    let source_url = item
        .get("source_url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let grounded_urls = evidence_urls(metadata);
    if !grounded_urls.contains_key(&normalize_evidence_url(source_url)) {
        return Err(OutcomeRejection::UngroundedContactResearch {
            reason: "source_url was not present in the task's bounded evidence set".to_owned(),
        });
    }

    let observed_on = item
        .get("observed_on")
        .and_then(Value::as_str)
        .and_then(parse_contact_research_day)
        .ok_or_else(|| OutcomeRejection::UngroundedContactResearch {
            reason: "observed_on is not a YYYY-MM-DD date".to_owned(),
        })?;
    let fact = item
        .get("fact")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let praise = item.get("praise").and_then(Value::as_str);
    let language = item
        .get("language")
        .and_then(Value::as_str)
        .unwrap_or("pl")
        .trim()
        .to_owned();
    if language.len() != 2 || !language.bytes().all(|byte| byte.is_ascii_lowercase()) {
        return Err(OutcomeRejection::UngroundedContactResearch {
            reason: "language must be a lowercase two-letter code".to_owned(),
        });
    }
    let hook = crowdrelay_domain::contact_research::PersonalHook::new(
        fact,
        praise,
        source_url,
        observed_on,
        OffsetDateTime::now_utc().date(),
    )
    .map_err(|refusal| OutcomeRejection::UngroundedContactResearch {
        reason: refusal.message(),
    })?;
    Ok((beacon_id, hook, language))
}

async fn persist_contact_research(
    tx: &mut Transaction<'_, Postgres>,
    outcome: &ValidatedOutcome,
    producing_task: Option<&(String, String, Value)>,
) -> Result<Uuid, AgentOutcomeError> {
    let (beacon_id, hook, language) =
        validate_contact_research_grounding(outcome, producing_task)?;

    let email = sqlx::query_scalar::<_, Option<String>>(
        r#"
        SELECT lower(btrim(contact_email))
        FROM beacons
        WHERE workspace_id=$1 AND id=$2 AND active
        "#,
    )
    .bind(outcome.workspace_id)
    .bind(beacon_id)
    .fetch_optional(&mut **tx)
    .await?
    .flatten()
    .filter(|value| !value.is_empty())
    .ok_or_else(|| OutcomeRejection::UngroundedContactResearch {
        reason: "the pinned Beacon no longer has an active contact address".to_owned(),
    })?;

    sqlx::query(
        r#"
        INSERT INTO contact_research
            (workspace_id, normalized_email, fact, praise, source_url, observed_on,
             language, researched_by)
        VALUES ($1,$2,$3,$4,$5,$6,$7,'agent:contact-research')
        ON CONFLICT (workspace_id, normalized_email, source_url) DO UPDATE
            SET fact=EXCLUDED.fact,
                praise=EXCLUDED.praise,
                observed_on=EXCLUDED.observed_on,
                language=EXCLUDED.language,
                researched_by=EXCLUDED.researched_by,
                researched_at=now()
        "#,
    )
    .bind(outcome.workspace_id)
    .bind(email)
    .bind(&hook.fact)
    .bind(&hook.praise)
    .bind(&hook.source_url)
    .bind(hook.observed_on)
    .bind(language)
    .execute(&mut **tx)
    .await?;

    Ok(beacon_id)
}
