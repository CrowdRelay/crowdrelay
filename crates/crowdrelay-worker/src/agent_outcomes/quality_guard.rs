// The data-quality guard that runs before any decision row is written.
//
// `include!`d into `agent_outcomes.rs` so it shares that module's scope.
// Split out to keep the parent inside the source-size ratchet.

/// Hard data-quality guard. Runs BEFORE the decision INSERT in `map_outcome`.
///
/// `require_approval` kinds create actions that reach external audiences or
/// fans — they must have evidence. `recommend_only` kinds (insights,
/// segments) are observations, not actions, so confidence 0 on them is a
/// weak observation rather than a dangerous act and they pass through.
///
/// For `OutreachTargets` specifically, the target must have a real identity
/// (display_name present and not the "Unnamed target" fallback) and at least
/// one evidence URL. A connector that returned nothing but errors produces
/// neither, and the LLM that hallucinated from empty data produces the
/// "Unnamed target" fallback — both are rejected here.
fn evaluate_outcome_quality(outcome: &ValidatedOutcome) -> Result<(), OutcomeRejection> {
    // Only require_approval kinds create actions. Insights and segments
    // are observations — confidence 0 is weak but not dangerous.
    if outcome.kind.disposition() != "require_approval" {
        return Ok(());
    }

    // What the RUN recorded, before anything the model wrote about itself.
    //
    // This is the check the item-level guards below cannot make. They read
    // fields the model authored, so a model answering confidently from a dead
    // connector clears all of them. The provenance block is authored by the
    // agents service from what actually happened: whether a second model
    // checked the output against the context, and whether that context loaded
    // at all.
    //
    // Fail-closed. A row with no provenance, an unreadable status, or an
    // unrecorded context is rejected rather than admitted, so an agents
    // deploy predating the contract shows up as a queue of explained
    // rejections instead of a stream of silent admissions.
    if let Err(rejection) = provenance_admission(outcome.kind, outcome.payload.provenance.as_ref())
    {
        return Err(OutcomeRejection::UnsupportedProvenance(rejection));
    }

    // The model's own report about its own output. A cheap filter that a
    // failing connector happens to trip, not a statement about evidence.
    if outcome
        .self_reported_confidence
        .self_reported_basis_points()
        == 0
    {
        return Err(OutcomeRejection::InsufficientEvidence {
            reason: "the model reported zero confidence in its own output".to_owned(),
        });
    }

    // A push is not an outreach contact: there is no external party to cite,
    // so evidence URLs would be a schema nobody could fill. Its equivalent
    // invariant is the destination — the one field deciding where a fan who
    // taps the notification ends up.
    if outcome.kind == OutcomeKind::SignalPush
        && let Some(item) = &outcome.payload.item
        && let Some(target) = item.get("target_path").and_then(Value::as_str)
    {
        let target = target.trim();
        if !target.is_empty() && !is_in_app_route(target) {
            return Err(OutcomeRejection::OffPlatformPushTarget {
                target: target.to_owned(),
            });
        }
    }

    // Outreach targets need a real identity and evidence URLs.
    if outcome.kind == OutcomeKind::OutreachTargets
        && let Some(item) = &outcome.payload.item
    {
        let display_name = item
            .get("display_name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        if display_name.is_empty() || display_name.eq_ignore_ascii_case("Unnamed target") {
            return Err(OutcomeRejection::MissingTargetIdentity);
        }
        let evidence = item.get("evidence_urls").cloned().unwrap_or(json!([]));
        let has_evidence = evidence.as_array().is_some_and(|items| {
            items.iter().any(|item| {
                // Evidence must be a non-empty string — not just a
                // non-null JSON value. A fabricated number or boolean
                // must not count as evidence.
                item.as_str().is_some_and(|s| !s.trim().is_empty())
            })
        });
        if !has_evidence {
            return Err(OutcomeRejection::InsufficientEvidence {
                reason: "no evidence URLs provided".to_owned(),
            });
        }
    }
    // No item at all is the honest empty: the emit side writes a single
    // item-less row when the model's items array came back empty — "I
    // looked, and nothing in the data was worth filing". That is a valid
    // observation, not a malformed one, and rejecting it counted a
    // correctly-behaving task as a failure in the execution-health
    // window (a misclassified honest answer drags dispatch budget down).
    // The malformed case — an item present but identity-less — still
    // rejects above.

    // Scout findings: the link is the finding and the kind is the contract.
    // A model that cannot supply both was not looking at a source.
    if outcome.kind == OutcomeKind::OpportunityFindings {
        let Some(item) = &outcome.payload.item else {
            return Err(OutcomeRejection::MissingFindingContent);
        };
        let kind = item
            .get("opportunity_kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if crowdrelay_domain::live_opportunities::ScoutOpportunityKind::parse(kind).is_none() {
            return Err(OutcomeRejection::InvalidFindingKind {
                kind: kind.to_owned(),
            });
        }
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
        if title.is_empty() || summary.is_empty() {
            return Err(OutcomeRejection::MissingFindingContent);
        }
        let link = item
            .get("destination_url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        if link.is_empty() || link.len() > 1_000 {
            return Err(OutcomeRejection::MissingFindingLink);
        }
        // `observed_at` is the source's own date when the source states one;
        // when absent the outcome's write time stands in. Either way the row
        // lands with an honest observation date, and a stale one simply shows
        // stale on the shortlist. Only a broken or impossible date is refused.
        parse_finding_timestamp(item.get("observed_at"))?;
    }

    // Strategy proposals: the item must carry a proposals array with at
    // least one element — a consultation that proposes nothing is not
    // advice. Per-proposal validity (unknown action, missing params,
    // out-of-bounds cadence) is NOT checked here: a bad proposal is a
    // rejected verdict the consultant learns from, not a poisoned outcome.
    if outcome.kind == OutcomeKind::StrategyProposals {
        let proposals_ok = outcome
            .payload
            .item
            .as_ref()
            .and_then(|item| item.get("proposals"))
            .and_then(Value::as_array)
            .is_some_and(|proposals| !proposals.is_empty());
        if !proposals_ok {
            return Err(OutcomeRejection::MissingProposalContent);
        }
    }

    Ok(())
}
