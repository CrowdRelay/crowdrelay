// The intelligence brief: one read answering "is the brain working, what mode
// is it in, what has it found, what is its plan, what does it need from you,
// what did it do, and what came of it."
//
// Composes the loaders the detail surfaces already run rather than
// duplicating their logic — the verdict, the strategy and the outcome counts
// are the same facts, not a second opinion of them. Every field is a fact or
// a count; the human-language narrative is derived from them at read time,
// never persisted. A section that fails fails the whole read — half a story
// is worse than a clear "could not answer."

/// What the brain's one-request story looks like.
#[derive(Debug, Serialize)]
pub(crate) struct IntelligenceBrief {
    /// Whether the machinery that runs the brain is alive, and how recently
    /// it completed a cycle or evaluated a decision.
    worker: WorkerSummary,
    /// The brain's verdict on its own trajectory — improving, learning,
    /// stagnant, regressing, or initializing — plus why it went quiet.
    brain: BrainSelfAssessment,
    /// The operating posture: grounded, working, or full send.
    posture: crowdrelay_application::autopilot::GrowthPostureView,
    /// What the brain would decide if a cycle ran right now — strategy,
    /// template priority, north star and what it expects to produce.
    cycle: crowdrelay_infra::autopilot::CyclePreview,
    /// What it found, did alone, parked, stopped, and what moved.
    chief_of_staff: crowdrelay_application::autopilot::AutopilotChiefOfStaff,
    /// Pending approvals with enough context to render a sentence and enough
    /// identity to approve. Community-relay batch deliveries are excluded —
    /// the In motion view owns those, so an operator is not asked twice.
    needs_you: Vec<PendingActionSummary>,
    /// Total pending approvals, including any beyond the list's page.
    awaiting_approval: i64,
    /// Communities the brain wants to reach but cannot — nobody joined them.
    blocked_communities: Vec<BlockedCommunity>,
    /// Finished work nobody published.
    unpublished_drafts: Vec<UnpublishedDraftChannel>,
}

/// Maps a repository error onto the operations error vocabulary so the
/// composed read can share one failure path.
fn repository_as_ops(error: crowdrelay_application::RepositoryError) -> OpsError {
    use crowdrelay_application::RepositoryError;
    match error {
        RepositoryError::Unavailable => OpsError::Unavailable,
        RepositoryError::NotFound => OpsError::NotFound,
        RepositoryError::Conflict | RepositoryError::ConflictBecause(_) => OpsError::Conflict,
        RepositoryError::Unexpected => OpsError::Unexpected,
    }
}

/// The intelligence brief — one request for the whole story.
///
/// Composes five ops-side loaders (worker, brain, needs_you, blocked,
/// drafts) and three repository reads (posture, cycle preview, chief of
/// staff) under the shared read budget. The ops loaders each pay one
/// permit; the repository reads pay the width their existing handlers
/// declare — the cycle preview fans out to five queries inside, so it
/// pays five.
pub async fn intelligence(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let timeout_duration = state.ops.operation_timeout;
    let budget = &state.read_budget;
    let workspace_id = state.ops.workspace_id();
    let now = OffsetDateTime::now_utc();

    let worker = run_limited(
        budget,
        timeout_duration,
        crate::ops_summary::load_worker_summary(&state.ops.pool),
    );
    let brain = run_limited(budget, timeout_duration, load_brain_assessment(&state.ops));
    let needs_you = run_limited(budget, timeout_duration, load_needs_you(&state.ops));
    let blocked = run_limited(
        budget,
        timeout_duration,
        load_blocked_communities(&state.ops),
    );
    let drafts = run_limited(
        budget,
        timeout_duration,
        load_unpublished_drafts(&state),
    );

    let posture = budgeted(
        budget,
        1,
        timeout_duration,
        state.autopilot.load_growth_posture(workspace_id),
    );
    let cycle = budgeted(
        budget,
        5,
        timeout_duration,
        crowdrelay_infra::autopilot::preview_autopilot_cycle(
            &state.autopilot,
            workspace_id,
            now,
        ),
    );
    let chief = budgeted(
        budget,
        1,
        timeout_duration,
        state
            .autopilot
            .load_chief_of_staff(workspace_id, now),
    );

    let (worker, brain, needs_you, blocked, drafts, posture, cycle, chief) = tokio::join!(
        worker, brain, needs_you, blocked, drafts, posture, cycle, chief,
    );

    let worker = match worker {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    let brain = match brain {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    let (needs_you, awaiting_approval) = match needs_you {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    let blocked = match blocked {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    let drafts = match drafts {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id(&headers)),
    };

    let posture = match posture {
        Some(Ok(value)) => value,
        Some(Err(error)) => {
            return repository_as_ops(error).into_response(request_id(&headers));
        }
        None => return OpsError::Unavailable.into_response(request_id(&headers)),
    };
    let cycle = match cycle {
        Some(Ok(value)) => value,
        Some(Err(error)) => {
            return repository_as_ops(error).into_response(request_id(&headers));
        }
        None => return OpsError::Unavailable.into_response(request_id(&headers)),
    };
    let chief = match chief {
        Some(Ok(value)) => value,
        Some(Err(error)) => {
            return repository_as_ops(error).into_response(request_id(&headers));
        }
        None => return OpsError::Unavailable.into_response(request_id(&headers)),
    };

    private_json(
        StatusCode::OK,
        IntelligenceBrief {
            worker,
            brain,
            posture,
            cycle,
            chief_of_staff: chief,
            needs_you,
            awaiting_approval,
            blocked_communities: blocked,
            unpublished_drafts: drafts,
        },
    )
}

#[cfg(test)]
mod intelligence_tests {
    use super::*;
    use crowdrelay_application::RepositoryError;

    #[test]
    fn repository_error_maps_to_ops_error() {
        // The composed read has one failure path — every repository error
        // must land on the right OpsError variant or the response would
        // misreport a timeout as an internal error.
        assert!(matches!(
            repository_as_ops(RepositoryError::Unavailable),
            OpsError::Unavailable
        ));
        assert!(matches!(
            repository_as_ops(RepositoryError::NotFound),
            OpsError::NotFound
        ));
        assert!(matches!(
            repository_as_ops(RepositoryError::Conflict),
            OpsError::Conflict
        ));
        assert!(matches!(
            repository_as_ops(RepositoryError::ConflictBecause("test")),
            OpsError::Conflict
        ));
        assert!(matches!(
            repository_as_ops(RepositoryError::Unexpected),
            OpsError::Unexpected
        ));
    }
}
