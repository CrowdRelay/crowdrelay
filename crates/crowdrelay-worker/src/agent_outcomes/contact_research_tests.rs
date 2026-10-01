#[test]
fn contact_research_must_match_the_brain_pinned_person_and_extracted_page_date() {
    let pinned = Uuid::now_v7();
    let source = "https://example.test/review";
    let observed = OffsetDateTime::now_utc().date();
    let outcome = tests::make_outcome(
        OutcomeKind::ContactResearch,
        8_000,
        Some(json!({
            "beacon_id": pinned.to_string(),
            "fact": "recenzja płyty „Szum” w audycji „Metalowy Wieczór”",
            "source_url": source,
            "observed_on": observed.to_string(),
            "language": "pl"
        })),
    );
    let task = (
        "contact-researcher".to_owned(),
        "research one relationship".to_owned(),
        json!({
            "subject_beacon_id": pinned.to_string(),
            "evidence": {
                "urls": [{
                    "url": source,
                    "tool": "research_contact",
                    "snippet": "exact page shown to the model"
                }]
            },
            "contact_research_pages": [{
                "url": source,
                "published_on": observed.to_string(),
                "recent": true
            }]
        }),
    );

    let (resolved, hook, language) =
        validate_contact_research_grounding(&outcome, Some(&task))
            .expect("the exact pinned page/date pair is grounded");
    assert_eq!(resolved, pinned);
    assert_eq!(hook.source_url, source);
    assert_eq!(hook.observed_on, observed);
    assert_eq!(language, "pl");
}

#[test]
fn contact_research_cannot_switch_to_another_beacon() {
    let pinned = Uuid::now_v7();
    let other = Uuid::now_v7();
    let source = "https://example.test/review";
    let observed = OffsetDateTime::now_utc().date();
    let outcome = tests::make_outcome(
        OutcomeKind::ContactResearch,
        8_000,
        Some(json!({
            "beacon_id": other.to_string(),
            "fact": "recenzja płyty „Szum” w audycji „Metalowy Wieczór”",
            "source_url": source,
            "observed_on": observed.to_string(),
            "language": "pl"
        })),
    );
    let task = (
        "contact-researcher".to_owned(),
        String::new(),
        json!({
            "subject_beacon_id": pinned.to_string(),
            "evidence": {"urls": [{"url": source}]},
            "contact_research_pages": [{
                "url": source,
                "published_on": observed.to_string(),
                "recent": true
            }]
        }),
    );

    assert!(matches!(
        validate_contact_research_grounding(&outcome, Some(&task)),
        Err(OutcomeRejection::UngroundedContactResearch { .. })
    ));
}

#[test]
fn contact_research_cannot_change_the_extractors_publication_date() {
    let pinned = Uuid::now_v7();
    let source = "https://example.test/review";
    let extracted = OffsetDateTime::now_utc().date();
    let claimed = extracted.previous_day().expect("previous day");
    let outcome = tests::make_outcome(
        OutcomeKind::ContactResearch,
        8_000,
        Some(json!({
            "beacon_id": pinned.to_string(),
            "fact": "recenzja płyty „Szum” w audycji „Metalowy Wieczór”",
            "source_url": source,
            "observed_on": claimed.to_string(),
            "language": "pl"
        })),
    );
    let task = (
        "contact-researcher".to_owned(),
        String::new(),
        json!({
            "subject_beacon_id": pinned.to_string(),
            "evidence": {"urls": [{"url": source}]},
            "contact_research_pages": [{
                "url": source,
                "published_on": extracted.to_string(),
                "recent": true
            }]
        }),
    );

    assert!(matches!(
        validate_contact_research_grounding(&outcome, Some(&task)),
        Err(OutcomeRejection::UngroundedContactResearch { .. })
    ));
}
