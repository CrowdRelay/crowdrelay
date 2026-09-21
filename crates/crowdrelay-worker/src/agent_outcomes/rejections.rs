/// Why an outcome was rejected by the data-quality guard. Stored in
/// `rejection_reason` for auditability — the row is never deleted.
///
/// The brain must never turn a connector failure into an opportunity.
/// A Reddit credential error produces 0 evidence and 0% confidence, and
/// without these guards that still became a decision with an
/// `awaiting_approval` action targeting "Unnamed target". NO EVIDENCE =
/// NO OPPORTUNITY.
#[derive(Debug, Eq, PartialEq)]
pub enum OutcomeRejection {
    /// confidence_basis_points is 0 for a require_approval kind.
    /// The connector produced no evidence to support a decision.
    InsufficientEvidence { reason: String },
    /// The outreach target has no usable identity — display_name is
    /// missing, empty, or the literal "Unnamed target" fallback.
    MissingTargetIdentity,
    /// The row's provenance bars it from becoming a pending action: nothing
    /// recorded how it was produced, it was never grounding-checked, or a
    /// data source behind it did not complete.
    ///
    /// Unlike every other variant here, this one reads what the agents
    /// service recorded about the RUN rather than what the model wrote in the
    /// item. That is the difference that matters: a model answering
    /// confidently from a dead connector clears every item-level test,
    /// because every item-level test reads fields the model authored.
    UnsupportedProvenance(ProvenanceRejection),
    /// A signal push whose deep link leaves the app.
    ///
    /// `target_path` is an in-app route (`/events/{id}`). A model that writes
    /// an absolute URL or a scheme there is proposing to send the fanbase to
    /// a destination nobody approved, and the approval click shows the copy,
    /// not the link.
    OffPlatformPushTarget { target: String },
    /// A community post whose target is not a screened-and-admitted
    /// community. The `target_id` a model supplies is only a claim — the row
    /// it names must exist, carry `screening_verdict = 'admitted'`, and sit
    /// at `status = 'promoted'`. Anything else is a post to a community
    /// nobody vetted: a fabricated UUID posts anywhere the model names, and
    /// a refused community (off-topic, too small, previously refused) posts
    /// past the screen that rejected it.
    UnvettedCommunity { target_id: Uuid },
    /// A community post that names no trusted video source — or names one
    /// that does not exist, is inactive, expired, or is not a video. A thread
    /// post exists to share a release video; anything else is a post about
    /// nothing — which is how fabricated anecdotes reached Reddit.
    UnsourcedPost { source_id: Option<String> },
    /// A community post whose relay batch the operator already answered:
    /// revoked means the spread was refused, done means the observation
    /// window closed. A draft landing after the answer is discarded, not
    /// parked — re-asking the same spread is the flood the batch exists to
    /// end.
    RelayBatchClosed { source_id: Uuid, status: String },
    /// A scout finding with no usable link. The destination is the finding —
    /// a title and a vibe with nothing to check is model memory wearing a
    /// finding's clothes.
    MissingFindingLink,
    /// A scout finding whose declared kind is outside the scout vocabulary.
    /// The kinds are the contract; a model inventing a ninth one is not
    /// extending the taxonomy, it is writing outside it.
    InvalidFindingKind { kind: String },
    /// A scout finding with no item, or one missing a usable title or
    /// summary. There is nothing to review when there is nothing to read.
    MissingFindingContent,
    /// A scout finding whose `observed_at` cannot be read or sits more than
    /// a day in the future. A stale date is not an error — it lands and the
    /// shortlist marks it stale — but a date that cannot be parsed or claims
    /// tomorrow is a broken finding.
    InvalidFindingTimestamp,
}

impl std::fmt::Display for OutcomeRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InsufficientEvidence { reason } => {
                write!(f, "INSUFFICIENT_EVIDENCE: {reason}")
            }
            Self::MissingTargetIdentity => {
                write!(
                    f,
                    "MISSING_TARGET_IDENTITY: display_name is missing or unnamed"
                )
            }
            Self::UnsupportedProvenance(rejection) => write!(f, "{rejection}"),
            Self::OffPlatformPushTarget { target } => write!(
                f,
                "OFF_PLATFORM_PUSH_TARGET: target_path {target:?} is not an in-app route"
            ),
            Self::UnvettedCommunity { target_id } => write!(
                f,
                "UNVETTED_COMMUNITY: target_id {target_id} is not an admitted, promoted community target"
            ),
            Self::UnsourcedPost { source_id } => write!(
                f,
                "UNSOURCED_POST: source_id {source_id:?} does not name an active, unexpired video content source for this workspace"
            ),
            Self::RelayBatchClosed { source_id, status } => write!(
                f,
                "RELAY_BATCH_CLOSED: relay batch for source {source_id} is {status} — the spread was already answered"
            ),
            Self::MissingFindingLink => write!(
                f,
                "MISSING_FINDING_LINK: a scout finding without a destination_url cannot be checked"
            ),
            Self::InvalidFindingKind { kind } => write!(
                f,
                "INVALID_FINDING_KIND: opportunity_kind {kind:?} is outside the scout vocabulary"
            ),
            Self::MissingFindingContent => write!(
                f,
                "MISSING_FINDING_CONTENT: a scout finding needs a title and a summary to review"
            ),
            Self::InvalidFindingTimestamp => write!(
                f,
                "INVALID_FINDING_TIMESTAMP: observed_at must be an RFC3339 timestamp no more than a day ahead"
            ),
        }
    }
}

impl std::error::Error for OutcomeRejection {}
