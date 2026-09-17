// The venue answer's assembly half (§12-1): the row's resolved global facts,
// the caller's own private ones, and the assessment each room carries on the
// city-venues read.
//
// Split out of `audience.rs` at the modularity contract's 1000-line chunk cap.
// The query that produces the rows stays with the other reads; what turns a
// row into a sentence lives here, next to the honesty rule it enforces — a
// room nobody could check does not read as a room with no evidence.

/// §12-1: the evidence answer on each room row — a verdict token plus one
/// sentence in the tenant's crew locale.
///
/// Every supporting read degrades quietly rather than failing the list: a
/// `crew_locale` miss is English (the source language of every sentence),
/// and a failed private-facts read narrows the sentence's evidence — it
/// never surfaces as a 500 over rows the main query already produced.
async fn assess_venue_rows(state: &crate::AppState, rows: &mut [CityVenueRow]) {
    if rows.is_empty() {
        return;
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let locale_tag = TenantSettingsRepository::new(state.database.clone())
        .crew_locale(workspace_id)
        .await
        .unwrap_or_default();
    let locale = EvidenceLocale::from_tag(&locale_tag);
    let (mut private_facts, private_facts_degraded) =
        private_venue_facts(state, workspace_id, rows).await;
    let now = OffsetDateTime::now_utc();
    for row in rows.iter_mut() {
        let evidence = VenueEvidence {
            display_name: row.display_name.clone(),
            city_name: row.city_name.clone(),
            facts: venue_row_facts(row),
            private_facts: private_facts.remove(&row.venue_id).unwrap_or_default(),
            shows_played: row.shows_played,
            shows_booked: row.shows_booked,
            repeat_attenders: row.repeat_attenders,
            comparable_acts: row.comparable_acts,
            typical_draw: row.typical_draw,
            last_played_at: row.last_played_at,
            next_show_at: row.next_show_at,
        };
        match assess(&evidence, now, locale) {
            VenueAssessment::WorthContact { sentence, .. } => {
                row.assessment = "worth_contact".to_owned();
                row.assessment_sentence = sentence;
            }
            VenueAssessment::InsufficientEvidence { sentence } => {
                // A refusal reached without the tenant's own facts is not a
                // refusal: a fresh booking contact is evidence, and the read
                // that holds it failed. Saying "no evidence" here would be a
                // confident wrong answer produced by an outage.
                if private_facts_degraded {
                    row.assessment = "not_assessed".to_owned();
                    row.assessment_sentence = unchecked_sentence(&row.display_name, locale);
                } else {
                    row.assessment = "insufficient_evidence".to_owned();
                    row.assessment_sentence = sentence;
                }
            }
        }
    }
}

/// Builds the `facts` half of a [`VenueEvidence`] out of the row's five
/// resolved `*_fact` triples. A triple missing its provenance or its
/// timestamp is not a fact — the second rule of §12-1 — so it contributes
/// nothing rather than half a claim.
fn venue_row_facts(row: &CityVenueRow) -> Vec<EvidenceFact> {
    let mut facts = Vec::new();
    let mut push = |attribute: &str,
                    value: &Option<String>,
                    provenance: &Option<String>,
                    observed_at: &Option<OffsetDateTime>| {
        if let (Some(value), Some(provenance), Some(observed_at)) = (value, provenance, observed_at)
        {
            facts.push(EvidenceFact {
                attribute: attribute.to_owned(),
                value: value.clone(),
                provenance: provenance.clone(),
                observed_at: *observed_at,
            });
        }
    };
    push(
        "capacity",
        &row.capacity_fact,
        &row.capacity_provenance,
        &row.capacity_observed_at,
    );
    push(
        "genres",
        &row.genres_fact,
        &row.genres_provenance,
        &row.genres_observed_at,
    );
    push(
        "website",
        &row.website_fact,
        &row.website_provenance,
        &row.website_observed_at,
    );
    push(
        "address",
        &row.address_fact,
        &row.address_provenance,
        &row.address_observed_at,
    );
    push(
        "status",
        &row.status_fact,
        &row.status_provenance,
        &row.status_observed_at,
    );
    facts
}

/// The tenant's own facts about the listed rooms — the half of the evidence
/// the shared read deliberately cannot show anyone else.
///
/// One query, resolved `DISTINCT ON (venue_id, attribute)` in the same
/// provenance trust order the global facts resolve in, so a private fact's
/// *age* can feed `BookingContactFresh` while its value stays off the row.
/// A failed read degrades to "no private facts": the clause simply never
/// forms rather than the list failing over it.
async fn private_venue_facts(
    state: &crate::AppState,
    workspace_id: Uuid,
    rows: &[CityVenueRow],
) -> (HashMap<Uuid, Vec<EvidenceFact>>, bool) {
    let venue_ids: Vec<Uuid> = rows.iter().map(|row| row.venue_id).collect();
    let result = sqlx::query_as::<_, PrivateVenueFactRow>(
        r#"
        SELECT DISTINCT ON (f.venue_id, f.attribute)
               f.venue_id, f.attribute, f.value, f.provenance, f.observed_at
        FROM place_venue_facts AS f
        WHERE f.workspace_id = $1
          AND f.venue_id = ANY($2)
          AND f.attribute IN ('booking_email', 'target_fit', 'contact_quality')
          AND (f.expires_at IS NULL OR f.expires_at > now())
        ORDER BY f.venue_id, f.attribute,
                 CASE f.provenance
                     WHEN 'played' THEN 0 WHEN 'researched' THEN 1
                     WHEN 'event_evidence' THEN 2 WHEN 'open_directory' THEN 3
                     ELSE 4 END,
                 f.observed_at DESC
        "#,
    )
    .bind(workspace_id)
    .bind(venue_ids)
    .fetch_all(&state.database)
    .await;
    let mut by_venue: HashMap<Uuid, Vec<EvidenceFact>> = HashMap::new();
    match result {
        Ok(facts) => {
            for fact in facts {
                by_venue
                    .entry(fact.venue_id)
                    .or_default()
                    .push(EvidenceFact {
                        attribute: fact.attribute,
                        value: fact.value,
                        provenance: fact.provenance,
                        observed_at: fact.observed_at,
                    });
            }
        }
        Err(error) => {
            tracing::warn!(%error, "venue private-facts read failed; assessing without it");
            return (by_venue, true);
        }
    }
    (by_venue, false)
}

