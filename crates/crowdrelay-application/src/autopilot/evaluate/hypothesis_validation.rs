/// How far back walk-forward validation reads.
///
/// The question validation asks is whether a template's edge survives out of
/// sample *now*, and a dispatch from a year ago is not evidence about now: the
/// audience, the roster of communities and the band's own standing have all
/// moved. Six months holds many cycles of every template while keeping the
/// per-cycle read bounded.
///
/// It also has to be comfortably wider than what the split needs. Validation
/// halves the range and purges 16 days across the boundary, so the window must
/// leave a usable out-of-sample half after that gap — 180 days does, by an
/// order of magnitude.
const VALIDATION_LOOKBACK_DAYS: i64 = 180;

/// Out-of-sample observations required before validation may move a template
/// at all. Below this, `validate_template` passes everything, so acting on the
/// result would be acting on the absence of evidence.
const MIN_OOS_OBSERVATIONS: u32 = 5;

/// The overfitting ceiling a degraded template must clear to be restored.
///
/// The same bar `validate_template` applies at 15+ out-of-sample observations,
/// borrowed here as hysteresis. Degrading asks only `!passed`; recovering asks
/// `passed` *and* this, so the two thresholds cannot both fire on the same
/// evidence and a template hovering at the boundary settles instead of
/// alternating. That matters because `passed` can change with no new evidence
/// at all: the split date is the midpoint of the evidence range and the range
/// slides as old rows age out of the lookback window.
const RECOVERY_OVERFITTING_CEILING_BPS: u16 = 7_000;

/// The lifecycle state validation justifies, or `None` to leave it alone.
///
/// Both directions, which is the point. This ran as a one-way ratchet:
/// `Active` → `Degraded` on a failed validation, and nothing anywhere that
/// could move it back. Production never calls `GrowthHypothesis::observe`, so
/// the recovery arm in the lifecycle policy was unreachable, and a template
/// that had one bad fortnight stayed at quarter budget permanently — a brain
/// that can lose confidence and cannot regain it, which over a long enough run
/// converges on doing nothing.
fn next_hypothesis_state(
    current: crowdrelay_brain::hypothesis::HypothesisState,
    result: &crowdrelay_brain::validation::WalkForwardResult,
) -> Option<crowdrelay_brain::hypothesis::HypothesisState> {
    use crowdrelay_brain::hypothesis::HypothesisState;

    if result.out_of_sample.observations < MIN_OOS_OBSERVATIONS {
        return None;
    }
    match (current, result.passed) {
        // Failed out of sample: quarter budget, and keep watching.
        (HypothesisState::Degraded | HypothesisState::Retired, false) => None,
        (_, false) => Some(HypothesisState::Degraded),
        // Recovered. Only from `Degraded` — `Retired` is the kill switch and
        // a validation window is not the thing that should reverse it.
        (HypothesisState::Degraded, true)
            if result.overfitting_score_bps < RECOVERY_OVERFITTING_CEILING_BPS =>
        {
            Some(HypothesisState::Active)
        }
        (_, true) => None,
    }
}

// Walk-forward validation and the hypothesis lifecycle — extracted from
// growth_intelligence_context.rs to keep both inside the source-size ratchet.
//
// One job: judge each worker template against the outcomes its own dispatches
// produced, degrade the ones whose edge did not survive out of sample, and
// restore the ones whose edge came back. Nothing here reaches candidate
// generation; the cycle reads the resulting `hypothesis_state` off the
// snapshots afterwards.
impl<R: AutopilotDecisionRepository> EvaluateAutopilot<'_, R> {
    /// Validates every template against its own resolved evidence and
    /// persists any lifecycle transition that validation justifies.
    ///
    /// Mutates `snapshots` in place so the rest of the cycle sees the state it
    /// will act under, rather than the one loaded before validation ran.
    async fn validate_hypotheses(
        &self,
        snapshots: &mut [crowdrelay_brain::GrowthIntelligenceSnapshot],
        now: OffsetDateTime,
    ) -> Result<(), AutopilotError> {
        // Walk-forward validation: load resolved evidence and validate
        // each template's out-of-sample performance. Templates that fail
        // validation are degraded from Active to Degraded, reducing their
        // dispatch budget; degraded ones that clear the recovery bar go back
        // to Active. See `next_hypothesis_state` for both directions and for
        // why they are not the same threshold. This is the wiring point
        // between the evidence persistence layer and the hypothesis lifecycle.
        //
        // The validation runs on treatment evidence only (control rows
        // have no observed outcome). The purge gap is 16 days to account
        // for Y30 durability overlap.
        //
        // Bounded to the last `VALIDATION_LOOKBACK_DAYS`. This ran with no
        // window at all: every resolved evidence row the workspace had ever
        // produced, read in full, every five minutes, against a table that
        // only grows. Free at thirty rows and a sequential scan of the whole
        // history at thirty thousand — a cycle that gets steadily more
        // expensive with nothing reporting it, which is the shape of a
        // problem nobody notices until it is the whole problem.
        let growth_evidence = self
            .repository
            .load_growth_evidence(
                self.workspace_id,
                Some(now - time::Duration::days(VALIDATION_LOOKBACK_DAYS)),
            )
            .await?;
        // Grouped once, by moving each row into the bucket its opportunity
        // names. The loop below used to re-scan the whole batch per template
        // and clone every match, so a workspace with T templates and E
        // resolved rows cloned O(T·E) times every five minutes, forever,
        // against a table that only grows.
        //
        // The key is the first segment of `template:target:action:hash`, not a
        // prefix of it. `starts_with(template_id)` matched any template whose
        // id begins with another's — no pair collides today, and the first
        // `social-post-v2` beside `social-post` would have silently validated
        // one template against the other's outcomes.
        let mut evidence_by_template: std::collections::HashMap<
            String,
            Vec<crowdrelay_brain::GrowthEvidence>,
        > = std::collections::HashMap::new();
        for evidence in growth_evidence {
            let Some(template) = evidence
                .opportunity_id
                .as_deref()
                .and_then(|id| id.split(':').next())
                .filter(|template| !template.is_empty())
                .map(str::to_owned)
            else {
                continue;
            };
            evidence_by_template.entry(template).or_default().push(evidence);
        }
        const NO_EVIDENCE: &[crowdrelay_brain::GrowthEvidence] = &[];
        for snapshot in snapshots.iter_mut() {
            let template_evidence = evidence_by_template
                .get(&snapshot.template_id)
                .map_or(NO_EVIDENCE, Vec::as_slice);
            let result =
                crowdrelay_brain::validation::validate_evidence_for_promotion(template_evidence);
            let previous_state = snapshot.hypothesis_state;
            let Some(new_state) = next_hypothesis_state(previous_state, &result) else {
                continue;
            };
            // Persist before acting on it. A snapshot moved ahead of a failed
            // write leaves this cycle sizing dispatches by a state the next
            // cycle will not load, and the layer has no logger to say so —
            // the durable record is the only belief either cycle can agree on.
            if self
                .repository
                .save_hypothesis_state(self.workspace_id, &snapshot.template_id, new_state)
                .await
                .is_err()
            {
                continue;
            }
            snapshot.hypothesis_state = new_state;
            // The revision is recorded only once the transition is durable.
            // A ledger entry for a state the next cycle will not load
            // describes learning that did not survive the cycle.
            self.record_hypothesis_revision(
                &snapshot.template_id,
                previous_state,
                new_state,
                &result,
                template_evidence,
            )
            .await;
        }
        Ok(())
    }

    /// Records a hypothesis lifecycle transition in the belief-revision
    /// ledger, citing the dispatches whose measured outcomes failed
    /// validation.
    ///
    /// Best-effort by design: the transition is already persisted when this
    /// runs, so a failure here costs the operator the explanation and costs
    /// the brain nothing.
    async fn record_hypothesis_revision(
        &self,
        template_id: &str,
        previous: crowdrelay_brain::hypothesis::HypothesisState,
        current: crowdrelay_brain::hypothesis::HypothesisState,
        result: &crowdrelay_brain::validation::WalkForwardResult,
        template_evidence: &[crowdrelay_brain::GrowthEvidence],
    ) {
        let caused_by: Vec<Uuid> = template_evidence
            .iter()
            .filter_map(|evidence| evidence.action_id)
            .collect();
        let Some(revision) = crate::autopilot::hypothesis_state_revision(
            template_id,
            previous,
            current,
            result.out_of_sample.observations,
            &caused_by,
        ) else {
            return;
        };
        let _ = self
            .repository
            .record_belief_revisions(self.workspace_id, std::slice::from_ref(&revision))
            .await;
    }
}

#[cfg(test)]
mod hypothesis_validation_tests {
    use super::next_hypothesis_state;
    use crowdrelay_brain::hypothesis::HypothesisState;
    use crowdrelay_brain::validation::{GrowthPerformanceRecord, WalkForwardResult};

    fn result(observations: u32, passed: bool, overfitting_score_bps: u16) -> WalkForwardResult {
        WalkForwardResult {
            in_sample: GrowthPerformanceRecord::default(),
            out_of_sample: GrowthPerformanceRecord {
                observations,
                mean_fans: if passed { 3.0 } else { -1.0 },
                std_fans: 1.0,
            },
            degradation_fans: 0.0,
            overfitting_score_bps,
            passed,
        }
    }

    /// The transition that already worked, kept working.
    #[test]
    fn a_template_that_fails_out_of_sample_is_degraded() {
        assert_eq!(
            next_hypothesis_state(HypothesisState::Active, &result(8, false, 9_000)),
            Some(HypothesisState::Degraded)
        );
    }

    /// The one that did not exist.
    ///
    /// Degrading was persisted and nothing ever wrote the state back, so the
    /// only durable direction was downward. A template earns quarter budget
    /// back by passing the same validation that took it away.
    #[test]
    fn a_degraded_template_that_passes_again_is_restored() {
        assert_eq!(
            next_hypothesis_state(HypothesisState::Degraded, &result(8, true, 1_000)),
            Some(HypothesisState::Active),
            "a degraded template with a clean out-of-sample window must be able \
             to recover; without this the brain can only ever lose confidence"
        );
    }

    /// Recovery is strictly harder than degradation, so the pair cannot flap.
    ///
    /// `passed` can change with no new evidence — the split date is the
    /// midpoint of the evidence range, and the range slides as rows age out of
    /// the lookback window. Symmetric thresholds would let that alone
    /// alternate a template between full and quarter budget.
    #[test]
    fn a_marginal_pass_does_not_undo_a_degradation() {
        let marginal = result(8, true, 8_000);
        assert_eq!(
            next_hypothesis_state(HypothesisState::Degraded, &marginal),
            None,
            "a pass that clears the mean test but not the overfitting bar must \
             leave the state where it is"
        );
        assert_eq!(
            next_hypothesis_state(HypothesisState::Active, &marginal),
            None,
            "and must not degrade an active one either — otherwise the band is \
             not hysteresis, it is a second threshold"
        );
    }

    /// Below the evidence floor, validation passes everything.
    #[test]
    fn too_little_out_of_sample_evidence_moves_nothing() {
        for state in [HypothesisState::Active, HypothesisState::Degraded] {
            assert_eq!(
                next_hypothesis_state(state, &result(4, true, 0)),
                None,
                "under five out-of-sample observations `passed` is true by \
                 default, so acting on it is acting on no evidence"
            );
        }
    }

    /// Retired is the kill switch. A validation window does not reverse it.
    #[test]
    fn validation_does_not_resurrect_a_retired_template() {
        assert_eq!(
            next_hypothesis_state(HypothesisState::Retired, &result(20, true, 0)),
            None
        );
        assert_eq!(
            next_hypothesis_state(HypothesisState::Retired, &result(20, false, 9_000)),
            None
        );
    }

    /// A state already at its destination is not a transition.
    #[test]
    fn an_already_degraded_template_is_not_degraded_again() {
        assert_eq!(
            next_hypothesis_state(HypothesisState::Degraded, &result(8, false, 9_000)),
            None,
            "re-degrading writes a belief revision describing no change"
        );
    }
}
