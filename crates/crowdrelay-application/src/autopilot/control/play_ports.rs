// The play ledger: what the agent committed to, what it did, and what that is
// allowed to prove. Read-only, and deliberately separate from the decision and
// action read models — those answer "what happened", this answers "what does it
// mean", and the two questions have different standards of evidence.

/// One claim about one play.
///
/// `claim` names the strength and `claim_means` spells it out in the response
/// itself. A number that travels without saying what it proves is how a tracker
/// count that rose during a campaign becomes "the campaign raised trackers"
/// somewhere downstream, and there is no schema comment that prevents that.
#[derive(Clone, Debug, Serialize)]
pub struct PlayClaimView {
    pub claim: PlayClaim,
    pub claim_means: &'static str,
    pub success_metric_platform: String,
    pub success_metric_key: String,
    pub window_start: OffsetDateTime,
    pub window_end: OffsetDateTime,
    /// `pending` until the window closes, then `succeeded` or `failed`.
    pub status: String,
    /// `measured` or `insufficient`. Absent while the claim is still pending.
    pub evidence: Option<String>,
    /// Present exactly when the evidence is `insufficient`. This is the field
    /// that keeps "we could not tell" from reading as "nothing happened".
    pub evidence_reason: Option<String>,
    /// Absent on an insufficient claim, and absent on a measured count that has
    /// nothing to be compared against.
    pub effect: Option<EffectAssessment>,
    /// Absent when the pre-play rate was too flat to carry a percentage.
    pub delta_basis_points: Option<i32>,
    pub baseline_milli_per_day: Option<i64>,
    pub observed_milli_per_day: Option<i64>,
    /// The denominator. An effect over nobody is not a null result.
    pub recipients_reached: Option<u32>,
}

/// One play, with what it claimed in advance and what it settled to.
#[derive(Clone, Debug, Serialize)]
pub struct PlayLedgerEntry {
    pub play_id: PlayId,
    pub kind: PlayKind,
    pub event_id: EventId,
    pub anchor_at: OffsetDateTime,
    /// Frozen when the play started, so the claim can be read back rather than
    /// reconstructed from whatever the code says today.
    pub hypothesis: String,
    pub state: String,
    pub started_at: OffsetDateTime,
    pub completed_at: Option<OffsetDateTime>,
    pub steps_total: u32,
    pub steps_settled: u32,
    /// Steps that will never be sent, and why they were not, are the numbers an
    /// operator should see first: they are the agent reporting its own gaps.
    pub steps_skipped: u32,
    pub recipients_reached: u32,
    pub claims: Vec<PlayClaimView>,
}

/// The ledger and the standings together.
///
/// Returned as one read because they answer one question. A list of campaigns
/// with no standings beside it invites the reader to conclude that a kind which
/// stopped appearing simply had no shows, when it may have retired itself.
#[derive(Clone, Debug, Serialize)]
pub struct PlayLedger {
    pub plays: Vec<PlayLedgerEntry>,
    pub standings: Vec<PlayKindStanding>,
}

#[async_trait]
pub trait AutopilotPlayLedgerRepository: Send + Sync {
    async fn load_play_ledger(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<PlayLedger, RepositoryError>;
}
