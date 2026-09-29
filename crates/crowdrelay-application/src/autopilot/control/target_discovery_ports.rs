// Ingress for the pitcher's supply. Kept apart from the other state ports
// because a candidate is not business state: it is an unverified claim about
// the outside world that has to survive screening before it becomes anything.
//
// The adapter calls Spotify and the directories; CrowdRelay stays the authority
// for candidates, screening, refusals and promotion. Nothing here fetches.

/// One candidate as an adapter found it. Every field is evidence, and the
/// screening rules in `crowdrelay_domain::target_discovery` decide what becomes
/// of it.
#[derive(Clone, Debug)]
pub struct IngestOutreachCandidate {
    pub target_kind: OutreachTargetKind,
    pub display_name: String,
    pub source: CandidateSource,
    /// The playlist, page or message the route was read out of.
    pub source_reference: String,
    /// The published text the route was read from, verbatim.
    pub evidence: Option<String>,
    pub route_kind: RouteKind,
    pub route_value: String,
    /// False means the route was worked out rather than read. Such a candidate
    /// is stored refused, so the same guess is not made again next week.
    pub route_is_published: bool,
    /// The submission channel this route belongs to, by slug. Its cost decides
    /// whether a pitch through it is contact or spend.
    pub channel_slug: Option<String>,
    pub fit_basis_points: u16,
    pub follower_count: Option<u32>,
    pub engagement_count: Option<u32>,
    pub sells_placement: bool,
    pub churns_indiscriminately: bool,
}

/// What the sweep read, as opposed to what it reported.
///
/// Optional throughout, because an adapter that cannot count what it read is
/// still allowed to report candidates; the supply rule then falls back to
/// treating a barren sweep as a dry source, which is what it did before this
/// existed. Present, it separates a source with nothing to give from a read
/// path that returned nothing at all.
///
/// These are adapter claims. They may only ever change which hold the supply
/// rule reports — never widen authority, raise a cap, or let a candidate skip
/// screening.
#[derive(Clone, Copy, Debug)]
pub struct OutreachSweepReport {
    /// Sources queried, e.g. one per configured search.
    pub sources_read: u32,
    /// Items those sources returned, before any screening.
    pub items_seen: u32,
}

/// What one batch did. Counts rather than rows: the adapter posts hundreds and
/// needs to know whether to keep going, not to read them back.
#[derive(Clone, Debug, Default, Serialize)]
pub struct OutreachCandidateIngestion {
    pub operation_id: uuid::Uuid,
    pub received: u32,
    /// Admitted for an operator to confirm the route.
    pub admitted: u32,
    /// Screened out, with the reason recorded on the row.
    pub refused: u32,
    /// Already known by contact identity. Re-finding a candidate is normal and
    /// must never re-screen or re-refuse it.
    pub duplicates: u32,
    pub replayed: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct OutreachCandidateView {
    pub id: uuid::Uuid,
    pub target_kind: &'static str,
    pub display_name: String,
    pub source: &'static str,
    pub source_reference: String,
    pub route_kind: &'static str,
    /// Deliberately absent from the list view. A screening queue is read far
    /// more often than it is acted on, and a contact route is not needed to
    /// decide whether the evidence is good.
    pub evidence: Option<String>,
    pub status: String,
    pub refusal_reason: Option<String>,
    pub pitch_class: Option<String>,
    pub fit_basis_points: i32,
    pub follower_count: Option<i32>,
}

/// The outcome of an operator confirming a candidate's route.
#[derive(Clone, Debug, Serialize)]
pub struct OutreachCandidatePromotion {
    pub operation_id: uuid::Uuid,
    pub candidate_id: uuid::Uuid,
    /// Absent where the route is a form or a handle: those are real published
    /// routes, but a target carries an address, so they wait for the pitcher
    /// that can use them rather than being thrown away.
    pub target_id: Option<OutreachTargetId>,
    pub replayed: bool,
}

#[derive(Clone, Debug)]
pub struct UpsertSubmissionChannel {
    pub slug: String,
    pub display_name: String,
    pub cost_model: ChannelCost,
    pub submission_url: Option<String>,
    pub active: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct SubmissionChannelMutation {
    pub operation_id: uuid::Uuid,
    pub channel_id: uuid::Uuid,
    pub version: i64,
    pub replayed: bool,
}

/// A proposed CRM contact as the import screen sees it: the agent's row plus
/// the verdict the deterministic screen assigns to it.
#[derive(Clone, Debug, Serialize)]
pub struct OutreachImportProposal {
    pub id: uuid::Uuid,
    pub target_kind: String,
    pub display_name: String,
    pub contact_email: Option<String>,
    pub contact_domain: Option<String>,
    pub why_fit: String,
    /// `admit`, or `refuse:` plus one of `invalid_email`, `already_target`,
    /// `do_not_contact`, `role_address`, `duplicate_in_batch`.
    pub verdict: String,
}

/// The screened page plus a count per verdict over every matching row, so
/// the operator sees the whole batch's shape and not only the page's.
#[derive(Clone, Debug, Default, Serialize)]
pub struct OutreachImportProposalPage {
    pub proposals: Vec<OutreachImportProposal>,
    pub verdict_counts: std::collections::BTreeMap<String, u32>,
}

/// What an approval covers: named rows, or every admitted row of one kind.
#[derive(Clone, Debug)]
pub enum OutreachImportSelection {
    Ids(Vec<uuid::Uuid>),
    AllAdmitted { target_kind: String },
}

/// What one approval call did.
#[derive(Clone, Debug, Serialize)]
pub struct OutreachImportApproval {
    pub operation_id: uuid::Uuid,
    /// Rows the screen admitted and this call promoted.
    pub admitted: u32,
    /// Rows it refused, counted under the verdict that refused them.
    pub refused_by_reason: std::collections::BTreeMap<String, u32>,
    /// Targets actually inserted — an address that already existed is not in
    /// here, because the conflict rule never touches an existing
    /// relationship.
    pub created_target_ids: Vec<uuid::Uuid>,
    pub replayed: bool,
}

/// The CRM-contact import gate: screen first, then bulk approve.
///
/// The agent's registry sweep proposes contacts faster than an operator can
/// confirm them one at a time, so this port answers two operator questions —
/// "what would happen to these" (list) and "promote the ones that pass"
/// (approve) — with the same deterministic screen behind both. Approval is
/// the consent point: nothing is contacted until a row is promoted, and the
/// bounded outreach lane still applies its own caps afterwards.
#[async_trait]
pub trait AutopilotOutreachImportRepository: Send + Sync {
    async fn list_outreach_import_proposals(
        &self,
        workspace_id: WorkspaceId,
        target_kind: Option<String>,
        limit: u32,
    ) -> Result<OutreachImportProposalPage, RepositoryError>;

    async fn approve_outreach_import_proposals(
        &self,
        workspace_id: WorkspaceId,
        selection: OutreachImportSelection,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<OutreachImportApproval, RepositoryError>;
}

#[async_trait]
pub trait AutopilotTargetDiscoveryRepository: Send + Sync {
    /// Screens and stores a bounded batch. Idempotent on the operation and
    /// deduplicated on contact identity, so a replayed batch and a re-found
    /// candidate are both no-ops rather than duplicates.
    async fn ingest_outreach_candidates(
        &self,
        workspace_id: WorkspaceId,
        candidates: Vec<IngestOutreachCandidate>,
        sweep_report: Option<OutreachSweepReport>,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<OutreachCandidateIngestion, RepositoryError>;

    async fn list_outreach_candidates(
        &self,
        workspace_id: WorkspaceId,
        status: Option<String>,
        limit: u32,
    ) -> Result<Vec<OutreachCandidateView>, RepositoryError>;

    /// Confirms the route an operator has checked, which is the only way a
    /// candidate becomes a target.
    async fn confirm_outreach_candidate(
        &self,
        workspace_id: WorkspaceId,
        candidate_id: uuid::Uuid,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<OutreachCandidatePromotion, RepositoryError>;

    async fn upsert_submission_channel(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertSubmissionChannel,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<SubmissionChannelMutation, RepositoryError>;
}
