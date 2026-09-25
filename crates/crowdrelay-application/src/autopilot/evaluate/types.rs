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

/// What one context did this cycle: how many candidates its detectors
/// produced and how far down the persist funnel each got.
///
/// `decisions` alone cannot report the two quiet failure modes. A candidate
/// the quota eats leaves no row anywhere, and a detector that produces
/// nothing is indistinguishable from one whose work all survived — both read
/// as "the context did nothing" when only one of them is healthy. Booking's
/// supply alarm was silent for weeks under a snapshot bug that read "never
/// requested" as "requested now"; a `candidates: 0` row against a live
/// policy is the reading that would have named it on the first cycle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextCycleActivity {
    pub context: AutopilotContext,
    /// The funnel top: candidates the context's detectors produced. A live
    /// policy whose detectors found nothing reports 0 here.
    pub candidates: u32,
    /// Candidates that survived to a decision row.
    pub decisions: u32,
    /// Candidates that left an action an executor can claim.
    pub actions: u32,
    /// Candidates the 24h quota ate — the one persist outcome that leaves no
    /// decision row at all, and therefore the one this count exists to name.
    pub throttled: u32,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
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
    /// Steps settled after sending — counted apart from skips because a
    /// delivered step is a fact the skip count must never describe.
    pub play_steps_delivered: u32,
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
    /// Decisions that would have been parked for a person, and were not,
    /// because the week's attention budget is spent.
    ///
    /// Its own counter rather than a share of `actions_held`, because it is
    /// the one number on this report an operator can act on directly: a
    /// cycle that withheld asks is the agent saying "I had more to show you
    /// than you have time for". The findings are all still on the board as
    /// recommendations — raising the budget surfaces them as asks again.
    pub asks_withheld: u32,
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
    /// The metric `north_star_observed` is in (`NorthStarMetric::as_str`), so
    /// a series of readings never mixes two metrics under one name.
    pub north_star_metric: Option<&'static str>,
    /// Diagnostic: how many GI candidates were scored before portfolio selection.
    pub gi_candidates: u32,
    /// Diagnostic: WAIT reason if the portfolio selected nothing.
    pub gi_wait_reason: Option<String>,
    /// Diagnostic: why the content-supply context stayed quiet this cycle —
    /// set only when the watcher ran and found no live material, so a quiet
    /// week records *that it had nothing to say* rather than reading like a
    /// machine that merely lost the portfolio vote.
    pub supply_wait_reason: Option<String>,
    /// Diagnostic: GI dispatch details for operator visibility.
    pub gi_dispatch_log: Vec<String>,
    /// Communities the brain wants to post to and cannot, because nobody has
    /// joined them, ranked by how many posts are waiting behind each.
    ///
    /// A prerequisite gate that removes candidates silently is indistinguishable
    /// from a brain with nothing to say. Production discovered 119 communities,
    /// joined none of them — the join executor is in manual mode — and every
    /// community candidate was dropped with no decision row and no operator
    /// signal. The whole Reddit acquisition channel read as idle when it was
    /// blocked on one manual step.
    ///
    /// Each entry is `(community, posts_waiting)`, most-wanted first. Empty
    /// when nothing is blocked, which is the healthy case.
    pub blocked_on_membership: Vec<(String, u32)>,
    /// Join-ask platforms the evaluator looked at and held this cycle —
    /// `(platform, hold)`. Same reason `blocked_on_membership` exists: a
    /// configured platform that stays quiet is work waiting on a person (no
    /// connection, no photo, no site URL), and a silent skip is
    /// indistinguishable from a detector that never ran.
    pub join_ask_held: Vec<(String, crowdrelay_domain::join_ask::JoinAskHold)>,
    /// Per-context funnel: every enabled context registers on entry, so a
    /// context that ran and produced nothing is distinguishable from one
    /// whose candidates were all eaten before a row existed.
    pub context_activity: std::collections::BTreeMap<AutopilotContext, ContextCycleActivity>,
}

impl AutopilotCycleReport {
    /// The funnel entry for one context, created on first touch — that is
    /// what lets a context report `candidates: 0` rather than being absent.
    fn context_stats(&mut self, context: AutopilotContext) -> &mut ContextCycleActivity {
        self.context_activity
            .entry(context)
            .or_insert_with(|| ContextCycleActivity {
                context,
                candidates: 0,
                decisions: 0,
                actions: 0,
                throttled: 0,
            })
    }
}

#[derive(Debug, Error)]
pub enum AutopilotError {
    #[error("autopilot repository failed: {0}")]
    Repository(#[from] RepositoryError),
    #[error("autopilot decision serialization failed")]
    Serialization(#[from] serde_json::Error),
}

pub struct EvaluateAutopilot<'a, R> {
    repository: &'a R,
    workspace_id: WorkspaceId,
}
