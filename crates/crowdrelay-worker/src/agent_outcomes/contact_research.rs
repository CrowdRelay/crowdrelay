// A researched fact about one person -> what the band knows about them.
//
// The rule this serves: nobody is written to as a stranger. CrowdRelay holds
// anybody without a recent, sourced fact on file (`contact_research`); the
// `contact-researcher` agent finds the fact, and this is where it is believed
// or refused.
//
// The model finds; it does not date and it does not source on its own say-so.
// A fact is accepted only when CrowdRelay can check every claim against what
// the model was actually shown:
//
//   * the person is the one the task was pinned to, by address, in metadata
//     written at dispatch. The item does not name a person at all: the model
//     has no way to attach a fact to somebody else;
//   * the cited URL is a page the research tool returned (the evidence the
//     context builder recorded), so a URL the model invented has no entry;
//   * the cited date is the publication date the tool recorded for that very
//     page — it sits next to the URL in the evidence snippet, read from the
//     page's own metadata. A model cannot make an old review look recent;
//   * the text passes the band's register (no hype, hashtags or links) and the
//     window (`crowdrelay_domain::contact_research::PersonalHook`).
//
// It lands as a row, never as an action: researching a person contacts nobody.

struct ValidatedResearch {
    email: String,
    hook: crowdrelay_domain::contact_research::PersonalHook,
    language: String,
}

fn ungrounded_research(reason: impl Into<String>) -> OutcomeRejection {
    OutcomeRejection::UngroundedContactResearch {
        reason: reason.into(),
    }
}

fn parse_iso_day(raw: &str) -> Option<time::Date> {
    let mut parts = raw.trim().splitn(3, '-');
    let year = parts.next()?.parse::<i32>().ok()?;
    let month = time::Month::try_from(parts.next()?.parse::<u8>().ok()?).ok()?;
    let day = parts.next()?.parse::<u8>().ok()?;
    time::Date::from_calendar_date(year, month, day).ok()
}

/// Whether the evidence snippet records `observed_on` as the page's own
/// `published_on`. The date has to follow that key closely: the same date in
/// the page's excerpt text proves nothing about when the page was published.
fn snippet_records_date(snippet: &str, observed_on: &str) -> bool {
    const KEY: &str = "published_on";
    snippet.match_indices(KEY).any(|(at, _)| {
        snippet
            .get(at + KEY.len()..)
            .unwrap_or_default()
            .chars()
            .take(24)
            .collect::<String>()
            .contains(observed_on)
    })
}

fn validate_contact_research(
    item: &Value,
    task_metadata: &Value,
    today: time::Date,
) -> Result<ValidatedResearch, OutcomeRejection> {
    let text = |key: &str| {
        item.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };

    // The person comes from the record of what was asked, never from the answer.
    let email = task_metadata
        .get("subject_contact_email")
        .and_then(Value::as_str)
        .map(|raw| raw.trim().to_ascii_lowercase())
        .filter(|raw| raw.contains('@'))
        .ok_or_else(|| ungrounded_research("the task was not pinned to a person"))?;

    let source_url = text("source_url").ok_or_else(|| ungrounded_research("source_url is missing"))?;
    let evidence = evidence_urls(task_metadata);
    let entry = evidence
        .get(&normalize_evidence_url(source_url))
        .ok_or_else(|| {
            ungrounded_research("source_url is not a page the research tool showed the model")
        })?;
    if entry.get("tool").and_then(Value::as_str) != Some("research_contact") {
        return Err(ungrounded_research(
            "source_url was not returned by the research tool",
        ));
    }

    let observed_raw =
        text("observed_on").ok_or_else(|| ungrounded_research("observed_on is missing"))?;
    let observed_on = parse_iso_day(observed_raw)
        .ok_or_else(|| ungrounded_research("observed_on is not a YYYY-MM-DD date"))?;
    let snippet = entry
        .get("snippet")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !snippet_records_date(snippet, observed_raw) {
        return Err(ungrounded_research(
            "observed_on is not the publication date the tool recorded for that page",
        ));
    }

    let fact = text("fact").ok_or_else(|| ungrounded_research("fact is missing"))?;
    let hook = crowdrelay_domain::contact_research::PersonalHook::new(
        fact,
        text("praise"),
        source_url,
        observed_on,
        today,
    )
    .map_err(|refusal| ungrounded_research(refusal.message()))?;

    let language = text("language")
        .filter(|code| code.len() == 2 && code.bytes().all(|b| b.is_ascii_lowercase()))
        .unwrap_or("pl")
        .to_owned();
    Ok(ValidatedResearch {
        email,
        hook,
        language,
    })
}

/// The decision's subject pair for a contact-research outcome. The outcome is
/// its own subject: the fact is keyed by an address, and a decision row must not
/// carry a person's address as its subject. An honest-empty envelope (the agent
/// found nothing it could stand behind) is allowed and lands the same way.
async fn contact_research_subject(
    tx: &mut Transaction<'_, Postgres>,
    outcome: &ValidatedOutcome,
    producing_task: Option<&(String, String, Value)>,
) -> Result<(&'static str, Uuid), AgentOutcomeError> {
    let Some(item) = &outcome.payload.item else {
        return Ok(("agent_outcome", outcome.id));
    };
    let Some((template, _, task_metadata)) = producing_task else {
        return Err(ungrounded_research("producing task is unavailable").into());
    };
    if template != "contact-researcher" {
        return Err(ungrounded_research(format!(
            "contact_research may only come from contact-researcher, got {template:?}"
        ))
        .into());
    }
    let validated =
        validate_contact_research(item, task_metadata, OffsetDateTime::now_utc().date())?;
    match crowdrelay_infra::contact_research::record_hook_on(
        tx,
        outcome.workspace_id,
        &validated.email,
        &validated.hook,
        &validated.language,
        "agent:contact-researcher",
    )
    .await
    {
        Ok(()) => Ok(("agent_outcome", outcome.id)),
        Err(crowdrelay_infra::contact_research::ResearchError::NotFound) => {
            Err(ungrounded_research("the task was not pinned to a usable address").into())
        }
        Err(crowdrelay_infra::contact_research::ResearchError::Refused(reason)) => {
            Err(ungrounded_research(reason).into())
        }
        Err(crowdrelay_infra::contact_research::ResearchError::Database(error)) => {
            Err(error.into())
        }
    }
}

#[cfg(test)]
mod contact_research_tests {
    use super::*;
    use serde_json::json;
    use time::macros::date;

    const EMAIL: &str = "redakcja@radio.example.test";
    const URL: &str = "https://radio.example.test/audycje/metalowy-wieczor";
    const TODAY: time::Date = date!(2026 - 10 - 02);

    fn metadata(published_on: &str) -> Value {
        json!({
            "subject_contact_email": EMAIL,
            "evidence": { "urls": [{
                "url": URL,
                "tool": "research_contact",
                "label": "Contact Research",
                "snippet": format!(
                    "\"url\": \"{URL}\",\n      \"published_on\": \"{published_on}\",\n      \"recent\": true"
                ),
                "fetched_at": "2026-10-02T10:00:00Z"
            }]}
        })
    }

    fn item() -> Value {
        json!({
            "type": "contact_research",
            "fact": "recenzja płyty „Szum” w audycji „Metalowy Wieczór”",
            "praise": "W recenzji „Szum” zwróciło nam uwagę, że weszliście w aranżację, a nie tylko brzmienie.",
            "source_url": URL,
            "observed_on": "2026-09-20",
            "language": "pl"
        })
    }

    fn rejected(item: &Value, metadata: &Value) -> String {
        match validate_contact_research(item, metadata, TODAY) {
            Err(OutcomeRejection::UngroundedContactResearch { reason }) => reason,
            other => panic!("expected a rejection, got {:?}", other.map(|v| v.email)),
        }
    }

    #[test]
    fn a_fact_the_tool_showed_and_dated_is_accepted() {
        let valid = validate_contact_research(&item(), &metadata("2026-09-20"), TODAY)
            .expect("grounded and in register");
        assert_eq!(valid.email, EMAIL);
        assert_eq!(valid.hook.observed_on, date!(2026 - 09 - 20));
        assert_eq!(valid.language, "pl");
        // The citation compares normalised: a trailing slash or a fragment is
        // the same page.
        let mut with_slash = item();
        with_slash["source_url"] = json!(format!("{URL}/#top"));
        assert!(validate_contact_research(&with_slash, &metadata("2026-09-20"), TODAY).is_ok());
    }

    /// The person is the one the task was pinned to, by address, and the
    /// answer cannot change it: whatever the item says about who it is for, the
    /// fact is filed against the pinned address and nobody else's.
    #[test]
    fn the_person_comes_from_the_record_of_what_was_asked() {
        let mut claims_somebody_else = item();
        claims_somebody_else["beacon_id"] = json!("0198f5a0-0000-7000-8000-000000000002");
        claims_somebody_else["contact_email"] = json!("ktos.inny@example.test");
        let valid = validate_contact_research(&claims_somebody_else, &metadata("2026-09-20"), TODAY)
            .expect("extra fields are ignored, not trusted");
        assert_eq!(valid.email, EMAIL);

        let mut unpinned = metadata("2026-09-20");
        unpinned.as_object_mut().unwrap().remove("subject_contact_email");
        assert!(rejected(&item(), &unpinned).contains("not pinned"));
        let mut not_an_address = metadata("2026-09-20");
        not_an_address["subject_contact_email"] = json!("nobody");
        assert!(rejected(&item(), &not_an_address).contains("not pinned"));
    }

    /// The model cannot cite a page it was not shown.
    #[test]
    fn a_source_the_model_never_saw_is_refused() {
        let mut invented = item();
        invented["source_url"] = json!("https://radio.example.test/audycje/wymyslone");
        assert!(rejected(&invented, &metadata("2026-09-20")).contains("showed the model"));
        let no_evidence = json!({ "subject_contact_email": EMAIL });
        assert!(rejected(&item(), &no_evidence).contains("showed the model"));
    }

    /// The model cannot make an old review look recent: the date it cites has
    /// to be the one the tool recorded for that page.
    #[test]
    fn the_date_is_the_one_recorded_for_the_page_not_the_models() {
        let mut generous = item();
        generous["observed_on"] = json!("2026-09-28");
        assert!(rejected(&generous, &metadata("2026-09-20")).contains("recorded for that page"));
        // Same date, but only in the page's excerpt text: that says nothing
        // about when the page was published.
        let mut in_prose = metadata("2026-03-01");
        in_prose["evidence"]["urls"][0]["snippet"] =
            json!(format!("\"url\": \"{URL}\", \"published_on\": \"2026-03-01\", \"excerpt\": \"... 2026-09-20 ...\""));
        assert!(rejected(&item(), &in_prose).contains("recorded for that page"));
    }

    #[test]
    fn only_the_research_tools_pages_count() {
        let mut elsewhere = metadata("2026-09-20");
        elsewhere["evidence"]["urls"][0]["tool"] = json!("web_search");
        assert!(rejected(&item(), &elsewhere).contains("research tool"));
    }

    /// Even a correctly dated, correctly sourced fact is refused when it is not
    /// the band's voice or not recent; these are the domain's rules, applied
    /// to the model's words exactly as to a person's.
    #[test]
    fn the_band_register_and_the_window_still_apply() {
        let mut hype = item();
        hype["praise"] = json!("Świetna robota!");
        assert!(rejected(&hype, &metadata("2026-09-20")).contains("exclamation"));

        let mut stale = item();
        stale["observed_on"] = json!("2026-03-01");
        assert!(rejected(&stale, &metadata("2026-03-01")).contains("not 'lately'"));

        let mut label = item();
        label["fact"] = json!("nowy odcinek");
        assert!(rejected(&label, &metadata("2026-09-20")).contains("too short"));
    }

    #[test]
    fn the_date_must_follow_the_published_on_key() {
        assert!(snippet_records_date("\"published_on\": \"2026-09-20\"", "2026-09-20"));
        assert!(!snippet_records_date("2026-09-20 \"published_on\": null", "2026-09-20"));
        assert!(!snippet_records_date("no key at all 2026-09-20", "2026-09-20"));
    }
}
