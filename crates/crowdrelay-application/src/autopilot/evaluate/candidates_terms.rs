// The terms-negotiation candidate, split out of `candidates.rs` to keep that
// file under the modularity contract's 1000-line chunk cap. Included rather
// than declared as a module: it shares the same free-function scope as every
// other candidate builder, and a module would re-declare imports the include
// already provides.

fn live_terms_candidate(
    snapshot: &LiveTermsSnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::LiveOpportunity(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let score = live_opportunity_score(snapshot.opportunity);
    let decision = evaluate_terms(
        snapshot.terms,
        snapshot.opportunity,
        *domain_policy,
        score,
        now,
    );
    let (decision_kind, reason, action) = match decision {
        // Declining and expiring are settlements written straight to the row,
        // not actions: the agent records that it will not take these terms and
        // telling the promoter stays a human act.
        TermsDecision::Hold | TermsDecision::Decline { .. } | TermsDecision::Expire => {
            return Ok(None);
        }
        // Report before ask: the first counter to a counterparty that is still
        // owed a post-show report waits until the report is out. The ask rides
        // on proof; sending the counter first spends the relationship on a
        // number the band has not yet evidenced.
        TermsDecision::Counter { round: 1, .. }
            if let Some(event_id) = snapshot.report_pending_event_id =>
        {
            (
                "issue_counterparty_report",
                "a post-show report is still owed to this counterparty, and the report goes \
                 out before the first ask does — the ask rides on that proof",
                AutopilotActionPayload::IssueCounterpartyReport {
                    opportunity_id: snapshot.terms.opportunity_id,
                    event_id,
                },
            )
        }
        TermsDecision::Counter { ask_minor, round } => (
            "counter_live_opportunity_terms",
            "the offer on the table is below what this show costs to play, and the counter is \
             the arithmetic rather than a guess",
            AutopilotActionPayload::CounterLiveOpportunityTerms {
                opportunity_id: snapshot.terms.opportunity_id,
                ask_minor,
                currency: snapshot.currency.clone(),
                round,
            },
        ),
        TermsDecision::Accept { fee_minor } => (
            "accept_live_opportunity_terms",
            "the fee on the table clears the band's own floor and the show breaks none of the \
             refusals that hold at every autonomy level",
            AutopilotActionPayload::AcceptLiveOpportunityTerms {
                opportunity_id: snapshot.terms.opportunity_id,
                fee_minor,
                currency: snapshot.currency.clone(),
            },
        ),
    };
    // Confidence is the opportunity's own, so a marginal show does not become a
    // confident negotiation by having a number attached to it.
    let confidence = snapshot.opportunity.evidence_confidence;
    let mut disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    // Every move here is third_party, and the class ceiling already downgrades
    // it. Forcing approval as well is belt and braces on the one action in the
    // system that commits the band's calendar and money at once.
    if matches!(disposition, PolicyDisposition::AutoExecute) {
        disposition = PolicyDisposition::RequireApproval;
    }
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::TeamOpportunity(snapshot.terms.opportunity_id),
        decision_kind,
        confidence,
        disposition,
        reason,
        input_snapshot: serde_json::json!({
            "terms": snapshot.terms,
            "opportunity": snapshot.opportunity,
            "score": score,
        }),
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action,
        // The round is in the key on purpose. A second ask is a second
        // decision, and one keyed only on the opportunity would be silently
        // deduplicated against the first.
        decision_key: format!(
            "decision:live-terms:v{}:{}:{}:{}",
            policy.version,
            snapshot.terms.opportunity_id,
            decision_kind,
            snapshot.terms.counter_rounds
        ),
        action_idempotency_key: format!(
            "action:live-terms:{}:{}:{}",
            snapshot.terms.opportunity_id, decision_kind, snapshot.terms.counter_rounds
        ),
    }))
}
