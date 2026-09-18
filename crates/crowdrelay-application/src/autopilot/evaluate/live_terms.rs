// The live-terms advance, split out of `evaluate.rs` to keep that file under
// the modularity contract's 1000-line parent cap. Included rather than
// declared as a module: it is one method on `EvaluateAutopilot`, and a module
// would need the whole generic bound restated to reach the same repository.

impl<'a, R> EvaluateAutopilot<'a, R>
where
    R: AutopilotDecisionRepository,
{
    /// Moves every live negotiation on by at most one step.
    ///
    /// Settlements are written straight to the row rather than queued as
    /// actions. A decline is the agent recording that it will not take these
    /// terms, and an unrecorded refusal reads to an operator exactly like the
    /// agent never looking.
    async fn advance_live_terms(
        &self,
        policy: &AutopilotPolicy,
        limits: &mut CycleLimits<'_>,
        report: &mut AutopilotCycleReport,
        now: OffsetDateTime,
    ) -> Result<(), AutopilotError> {
        let AutopilotPolicyConfig::LiveOpportunity(domain_policy) = policy.config else {
            return Ok(());
        };
        for snapshot in self
            .repository
            .load_live_opportunity_terms(self.workspace_id, now)
            .await?
        {
            let score = live_opportunity_score(snapshot.opportunity);
            match evaluate_terms(
                snapshot.terms,
                snapshot.opportunity,
                domain_policy,
                score,
                now,
            ) {
                TermsDecision::Hold => {}
                TermsDecision::Decline { reason } => {
                    self.repository
                        .settle_live_opportunity_terms(
                            self.workspace_id,
                            &TermsSettlement {
                                opportunity_id: snapshot.terms.opportunity_id,
                                state: TermsState::Declined,
                                reason: Some(reason),
                            },
                            now,
                        )
                        .await?;
                    report.terms_settled = report.terms_settled.saturating_add(1);
                }
                TermsDecision::Expire => {
                    self.repository
                        .settle_live_opportunity_terms(
                            self.workspace_id,
                            &TermsSettlement {
                                opportunity_id: snapshot.terms.opportunity_id,
                                state: TermsState::Expired,
                                reason: None,
                            },
                            now,
                        )
                        .await?;
                    report.terms_settled = report.terms_settled.saturating_add(1);
                }
                TermsDecision::Counter { .. } | TermsDecision::Accept { .. } => {
                    if let Some(candidate) = live_terms_candidate(&snapshot, policy, now)? {
                        self.persist(&candidate, limits, report).await?;
                    }
                }
            }
        }
        Ok(())
    }
}
