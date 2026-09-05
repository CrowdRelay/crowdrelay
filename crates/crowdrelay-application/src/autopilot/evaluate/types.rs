/// The cycle-wide limits every candidate is measured against.
///
/// Carried as one value because they are read together and, in the envelope's
/// case, spent together: passing them separately through a call chain is how a
/// budget ends up topped up on one path and not on another.
struct CycleLimits<'a> {
    ceilings: &'a [(ActionClass, AutonomyLevel)],
    envelope: &'a GrowthEnvelope,
    usage: &'a mut EnvelopeUsage,
    touch_ages: &'a std::collections::HashMap<uuid::Uuid, u32>,
    touched_this_cycle: &'a mut std::collections::HashSet<uuid::Uuid>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AutopilotCycleReport {
    pub decisions: u32,
    pub actions_enqueued: u32,
    pub actions_throttled: u32,
    /// Campaigns the agent committed to this cycle. Counted apart from
    /// decisions because starting a play emits nothing: it is the agent taking
    /// on work, and an operator should be able to see that happen before the
    /// first message goes anywhere.
    pub plays_started: u32,
    /// Steps settled without being sent. The number that matters most on this
    /// report: it is the agent saying what it did not do, which is the fact
    /// every other counter here would otherwise hide.
    pub play_steps_skipped: u32,
    pub plays_completed: u32,
    /// Claimed placements that reached an answer — confirmed and gone, or never
    /// confirmed at all. Counted apart from anything else because a placement
    /// that cannot be verified must never reach a result.
    pub placements_settled: u32,
    /// Free-reach waves opened, closed for review, and ended without ever
    /// reaching a human. The last is the one worth watching: it is the agent
    /// saying it drafted work nobody got to.
    pub waves_opened: u32,
    pub waves_sealed: u32,
    pub waves_expired: u32,
    /// Negotiations ended without an acceptance — declined for a stated reason,
    /// or expired because the promoter stopped waiting. Settlements rather than
    /// actions, so nothing else on this report would show them.
    pub terms_settled: u32,
    /// Decisions the volume envelope held back — the agent was switched off,
    /// rehearsing, out of budget, or inside a subject's cooldown.
    pub actions_held: u32,
    /// Decisions the class ceiling lowered — an action the context was willing
    /// to take unattended that now waits for a human. Counted separately from
    /// quota throttling because the two mean different things: throttled work
    /// is deferred, gated work is somebody's decision to make.
    pub actions_gated: u32,
    /// The North Star as this cycle read it, whatever metric the tenant has
    /// chosen — signal installs by default, not fans.
    ///
    /// Reported rather than re-derived. The cycle record used to compute its
    /// own figure with a hardcoded count of active fans, so a tenant whose
    /// North Star was anything else had two numbers under the same name: one on
    /// `/autopilot/cycle/preview`, another on `/ops/cycles`, and the brain's
    /// self-assessment trending the one the brain was not optimizing.
    ///
    /// `None` when the evaluation phase could not run. A cycle that took no
    /// reading records no reading, because a zero here is indistinguishable
    /// from having lost the entire audience.
    pub north_star_observed: Option<u32>,
}

#[derive(Debug, Error)]
pub enum AutopilotError {
    #[error("autopilot repository failed")]
    Repository(#[from] RepositoryError),
    #[error("autopilot decision serialization failed")]
    Serialization(#[from] serde_json::Error),
}
