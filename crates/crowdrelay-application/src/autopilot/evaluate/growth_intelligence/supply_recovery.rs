//! Recovery for a starved acquisition action space.
//!
//! This module never changes candidate value and never publishes or contacts
//! anyone. It only makes the existing read-only fanbase scout eligible sooner
//! when the caller has established that a verified-organic monthly target is
//! behind and fewer acquisition candidates survived the normal gates than the
//! cycle has capacity for. The scout's findings still pass normal screening
//! before any later action can use them.

use super::*;
use crowdrelay_domain::{autonomy::PolicyDisposition, worker_template::WorkerTemplate};

/// Successful research may be pulled forward from weekly cadence, but never
/// more often than daily solely because acquisition supply is short.
const SUPPLY_RECOVERY_COOLDOWN_HOURS: u32 = 24;

#[must_use]
fn recovery_cadence_ready(hours_since_last_run: Option<u32>) -> bool {
    hours_since_last_run.unwrap_or(u32::MAX) >= SUPPLY_RECOVERY_COOLDOWN_HOURS
}

/// What survived the normal acquisition gates versus how much room the cycle
/// currently has. Capacity comes from the same helper the portfolio uses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct AcquisitionSupply {
    pub available: u32,
    pub capacity: u32,
}

impl AcquisitionSupply {
    #[must_use]
    pub(super) fn assess(
        candidates: &[ScoredCandidate],
        recovery_enabled: bool,
        capacity: u32,
    ) -> Option<Self> {
        recovery_enabled.then(|| Self {
            available: u32::try_from(
                candidates
                    .iter()
                    .filter(|candidate| {
                        candidate.candidate.disposition == PolicyDisposition::AutoExecute
                            && candidate_template_id(candidate)
                                .and_then(WorkerTemplate::parse)
                                .is_some_and(WorkerTemplate::can_acquire_new_fans)
                    })
                    .count(),
            )
            .unwrap_or(u32::MAX),
            capacity: capacity.max(1),
        })
    }

    #[must_use]
    pub(super) const fn shortfall(self) -> u32 {
        self.capacity.saturating_sub(self.available)
    }

    #[must_use]
    pub(super) const fn needs_recovery(self) -> bool {
        self.shortfall() > 0
    }

    #[must_use]
    pub(super) fn summary(self) -> String {
        format!(
            "organic acquisition supply: {} eligible acquisition candidate(s) for {} dispatch slot(s), shortfall {}",
            self.available,
            self.capacity,
            self.shortfall(),
        )
    }
}

fn candidate_template_id(candidate: &ScoredCandidate) -> Option<&str> {
    match &candidate.candidate.action {
        AutopilotActionPayload::RequestAgentRun { template_id, .. } => Some(template_id.as_str()),
        _ => None,
    }
}

#[must_use]
pub(super) fn contains_template(candidates: &[ScoredCandidate], template_id: &str) -> bool {
    candidates
        .iter()
        .any(|candidate| candidate_template_id(candidate) == Some(template_id))
}

/// Pulls the read-only fanbase scout forward once when supply is short.
///
/// Reuses the existing rescan path rather than inventing a second cadence
/// implementation. That preserves every normal eligibility gate:
/// retired/discovered hypotheses do not act, standing still applies, and the
/// failed-run retry delay prevents a five-minute storm. The generated request
/// stays inside the normal portfolio; shortage does not guarantee selection.
#[allow(clippy::too_many_arguments)]
pub(super) fn supply_recovery_scout_candidate(
    snapshot: &GrowthIntelligenceSnapshot,
    supply: AcquisitionSupply,
    policy: &AutopilotPolicy,
    evidence: ContextEvidence,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
    causal_model: &CausalModel,
    strategy: GrowthStrategy,
    novelty: f64,
) -> Result<Option<ScoredCandidate>, serde_json::Error> {
    if !supply.needs_recovery()
        || snapshot.template_id != "fanbase-scout"
        || !recovery_cadence_ready(snapshot.hours_since_last_run)
    {
        return Ok(None);
    }

    let mut forced = snapshot.clone();
    forced.rescan_requested = true;
    let mut blocked = Vec::new();
    let mut unread = Vec::new();
    let mut generated = growth_intelligence_candidate(
        &mut blocked,
        &mut unread,
        &forced,
        policy,
        evidence,
        workspace_id,
        now,
        causal_model,
        strategy,
        novelty,
    )?;
    let Some(mut candidate) = generated.pop() else {
        return Ok(None);
    };
    if candidate_template_id(&candidate) != Some("fanbase-scout")
        || candidate.candidate.disposition != PolicyDisposition::AutoExecute
    {
        return Ok(None);
    }

    candidate.candidate.reason =
        "Verified organic growth is behind and acquisition supply is below cycle capacity";
    if let Some(input) = candidate.candidate.input_snapshot.as_object_mut() {
        // The bypass is internal control, not an operator/consultant rescan
        // request. Preserve the real snapshot fact and record the recovery
        // reason separately so provenance never claims a request that did not exist.
        if let Some(recorded_snapshot) = input
            .get_mut("snapshot")
            .and_then(serde_json::Value::as_object_mut)
        {
            recorded_snapshot.insert(
                "rescan_requested".to_owned(),
                serde_json::json!(snapshot.rescan_requested),
            );
        }
        input.insert(
            "supply_recovery".to_owned(),
            serde_json::json!({
                "available_acquisition_candidates": supply.available,
                "dispatch_capacity": supply.capacity,
                "shortfall": supply.shortfall(),
                "recovery_template": "fanbase-scout",
                "read_only": true,
            }),
        );
    }
    if let AutopilotActionPayload::RequestAgentRun { prompt, .. } = &mut candidate.candidate.action
    {
        prompt.push_str(&format!(
            "\n\nSUPPLY RECOVERY MODE: only {} acquisition candidate(s) survived the current gates for {} available dispatch slot(s). Find genuinely new, evidence-backed public fan-bearing places that can replenish the action space. This remains research only: do not contact people, do not post, and do not lower the fit or evidence bar because supply is short.",
            supply.available, supply.capacity,
        ));
    }
    Ok(Some(candidate))
}

/// Applies the supply-recovery control without bloating the context orchestrator.
///
/// The caller decides whether recovery is enabled from the already-computed goal
/// constraint. This function only compares surviving acquisition candidates with
/// that same cycle's dispatch capacity and, when starved, makes the existing
/// read-only fanbase scout eligible through its normal rescan path.
#[allow(clippy::too_many_arguments)]
pub(in crate::autopilot::evaluate) fn maybe_replenish_acquisition_supply(
    snapshots: &[GrowthIntelligenceSnapshot],
    candidates: &mut Vec<ScoredCandidate>,
    recovery_enabled: bool,
    capacity: u32,
    policy: &AutopilotPolicy,
    evidence: ContextEvidence,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
    causal_model: &CausalModel,
    strategy: GrowthStrategy,
    exploration_memory: &crowdrelay_brain::ExplorationMemory,
    report: &mut AutopilotCycleReport,
) -> Result<(), serde_json::Error> {
    let Some(supply) = AcquisitionSupply::assess(candidates, recovery_enabled, capacity) else {
        return Ok(());
    };
    report.gi_dispatch_log.push(supply.summary());
    if !supply.needs_recovery() || contains_template(candidates, "fanbase-scout") {
        return Ok(());
    }

    let Some(scout) = snapshots
        .iter()
        .find(|snapshot| snapshot.template_id == "fanbase-scout")
    else {
        report.gi_dispatch_log.push(format!(
            "organic acquisition supply recovery unavailable: no fanbase-scout snapshot; shortfall={}",
            supply.shortfall(),
        ));
        return Ok(());
    };
    let context = build_dispatch_context(scout, now);
    let novelty = exploration_memory.novelty("fanbase-scout", &context_hash(&context));
    match supply_recovery_scout_candidate(
        scout,
        supply,
        policy,
        evidence,
        workspace_id,
        now,
        causal_model,
        strategy,
        novelty,
    )? {
        Some(candidate) => {
            report.gi_dispatch_log.push(format!(
                "organic acquisition supply recovery eligible: fanbase-scout pulled forward; shortfall={}",
                supply.shortfall(),
            ));
            candidates.push(candidate);
        }
        None => report.gi_dispatch_log.push(format!(
            "organic acquisition supply recovery held: fanbase-scout failed-run retry/standing/hypothesis gate; shortfall={}",
            supply.shortfall(),
        )),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supply_recovery_is_daily_not_hourly() {
        assert!(!recovery_cadence_ready(Some(1)));
        assert!(!recovery_cadence_ready(Some(23)));
        assert!(recovery_cadence_ready(Some(24)));
        assert!(recovery_cadence_ready(None));
    }

    #[test]
    fn shortage_math_never_goes_negative() {
        let short = AcquisitionSupply {
            available: 2,
            capacity: 7,
        };
        assert_eq!(short.shortfall(), 5);
        assert!(short.needs_recovery());

        let full = AcquisitionSupply {
            available: 9,
            capacity: 7,
        };
        assert_eq!(full.shortfall(), 0);
        assert!(!full.needs_recovery());
    }
}
