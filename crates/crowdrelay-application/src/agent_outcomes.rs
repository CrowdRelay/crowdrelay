//! Serde mirror + validation for the `agent_outcomes` handoff table.
//!
//! The agents service (TypeScript) writes rows with `kind`, `schema_version`,
//! `payload` (JSONB), and `confidence_basis_points`. This module is the Rust
//! side of that contract: it deserializes, validates, and maps an outcome to
//! the autopilot decision/action rows the worker should insert.
//!
//! Ownership: the agents service is the only writer of `agent_outcomes`; the
//! Rust `AgentOutcomeWorker` is the only reader/mapper. Schema drift is
//! bounded by `schema_version` — unknown versions are rejected (never
//! deleted) with a clear `rejection_reason`.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The eight outcome kinds the agents service may emit. Mirrors the zod enum
/// in `crowdrelay-agents/src/agent/structured.ts`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeKind {
    PressPitch,
    SocialPost,
    SignalPush,
    AudienceSegments,
    OutreachTargets,
    CampaignInsight,
    ReleasePlanNote,
    GenericInsight,
}

impl OutcomeKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PressPitch => "press_pitch",
            Self::SocialPost => "social_post",
            Self::SignalPush => "signal_push",
            Self::AudienceSegments => "audience_segments",
            Self::OutreachTargets => "outreach_targets",
            Self::CampaignInsight => "campaign_insight",
            Self::ReleasePlanNote => "release_plan_note",
            Self::GenericInsight => "generic_insight",
        }
    }

    /// Autopilot context the worker assigns to the decision row.
    #[must_use]
    pub const fn autopilot_context(self) -> &'static str {
        match self {
            Self::PressPitch | Self::SocialPost => "promotion_budget",
            Self::SignalPush | Self::AudienceSegments => "fan_lifecycle",
            Self::OutreachTargets => "booking_opportunity",
            Self::CampaignInsight | Self::ReleasePlanNote | Self::GenericInsight => {
                "growth_intelligence"
            }
        }
    }

    /// `require_approval` kinds produce an `awaiting_approval` action;
    /// `recommend_only` kinds surface on the board without an action.
    #[must_use]
    pub const fn disposition(self) -> &'static str {
        match self {
            Self::PressPitch | Self::SocialPost | Self::SignalPush | Self::OutreachTargets => {
                "require_approval"
            }
            Self::AudienceSegments
            | Self::CampaignInsight
            | Self::ReleasePlanNote
            | Self::GenericInsight => "recommend_only",
        }
    }

    /// Decision kind written to `viryaos_autopilot_decisions.decision_kind`.
    #[must_use]
    pub const fn decision_kind(self) -> &'static str {
        match self {
            Self::PressPitch | Self::SocialPost => "agent_content_proposal",
            Self::SignalPush => "agent_signal_push_proposal",
            Self::AudienceSegments => "agent_segment_proposal",
            Self::OutreachTargets => "agent_target_proposal",
            Self::CampaignInsight | Self::ReleasePlanNote | Self::GenericInsight => "agent_insight",
        }
    }
}

/// What the grounding check established about an outcome.
///
/// Mirrors `VerificationStatus` in `crowdrelay-agents/src/agent/verify.ts`.
/// The verifier is a second LLM holding the prompt, the same data sections,
/// and the first model's output — no tools, no network. It answers exactly
/// one question: does the output assert things the supplied context does not
/// support? It does not, and cannot, establish external-world truth.
///
/// `NotVerified` is the `#[serde(other)]` arm on purpose. An unknown string,
/// a future status this build does not understand, or a status field the
/// agents service has not started writing yet all land on "no check
/// happened" rather than on a pass. Reading forward must never invent a
/// clean bill of health.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    /// A verifier ran and found no unsupported claim. NOT a truth claim.
    GroundingCheckPassed,
    /// A verifier ran and named concrete problems.
    GroundingCheckRejected,
    /// No check happened: no verifier, a failed call, an unparseable
    /// verdict, a deadline, or a payload this build cannot read.
    #[default]
    #[serde(other)]
    NotVerified,
}

/// Where the number in `agent_outcomes.confidence_basis_points` came from.
///
/// Today there is exactly one producer and it is a language model scoring its
/// own output. The variant exists so that a future measured source has
/// somewhere to go without silently inheriting this one's meaning; anything
/// unrecognised is `Unknown`, which the admission rules treat as
/// self-reported.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceSource {
    /// The model's own estimate of its own output. Measures nothing.
    ModelSelfReport,
    #[default]
    #[serde(other)]
    Unknown,
}

/// Grounding-check result carried on the outcome row.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub struct VerificationProvenance {
    #[serde(default)]
    pub status: VerificationStatus,
}

/// How complete the context behind the outcome was.
///
/// Both flags default to the pessimistic value. A row whose provenance
/// predates this contract, or whose fields this build cannot read, is treated
/// as having been built on a degraded context — the safe reading, because the
/// alternative is asserting completeness nobody recorded.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct ContextProvenance {
    #[serde(default = "yes")]
    pub any_source_failed: bool,
    #[serde(default = "yes")]
    pub any_source_truncated: bool,
}

const fn yes() -> bool {
    true
}

impl Default for ContextProvenance {
    fn default() -> Self {
        Self {
            any_source_failed: true,
            any_source_truncated: true,
        }
    }
}

/// What the confidence number on the row actually is.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub struct ConfidenceProvenance {
    #[serde(default)]
    pub source: ConfidenceSource,
}

/// Which model actually produced the text, after fallback.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ModelProvenance {
    #[serde(default)]
    pub actual: String,
    #[serde(default)]
    pub provider: Option<String>,
    /// Immutable identity of the template revision that built the prompt.
    #[serde(default)]
    pub template_revision: Option<String>,
}

/// The epistemic record the agents service writes with every outcome.
///
/// Deliberately a SUBSET of `payload.provenance` as the agents service emits
/// it: only the fields an admission or recycling decision turns on are
/// mirrored here. The rest (per-source row counts, char budgets, attempt
/// counts) stays in the JSONB for forensics and is not part of this contract.
/// Widening it means widening what the Rust side promises to honour.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct OutcomeProvenance {
    #[serde(default)]
    pub verification: VerificationProvenance,
    #[serde(default)]
    pub context: ContextProvenance,
    #[serde(default)]
    pub confidence: ConfidenceProvenance,
    #[serde(default)]
    pub model: ModelProvenance,
}

/// The envelope row as written by the agents service. The worker reads
/// `payload` as a JSONB object and extracts `rationale` + optional `item`
/// from it; the envelope-level row (no items) stores `{ rationale, kind }`,
/// item rows store `{ item, rationale }`.
///
/// `provenance` is `Option` because rows written before the contract existed
/// carry none. Absent provenance is not neutral: [`provenance_admission`]
/// treats it as unverified and degraded.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OutcomePayload {
    #[serde(default)]
    pub rationale: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<OutcomeProvenance>,
}

/// A number a language model wrote about its own output.
///
/// It is not empirical, not statistical, and not derived from any
/// measurement. A model with no supporting data emits 9500 as readily as a
/// model reading perfect data emits 3000.
///
/// This type exists because the value used to travel as a bare `i32` into
/// `viryaos_autopilot_decisions.confidence_basis_points`, the column the
/// deterministic paths fill from `evidence_confidence` and that
/// `next_best_action` parses into [`crowdrelay_domain::Confidence`]. Same
/// column, same range, same parser — so a self-report ranked beside a
/// measurement and nothing in the type system objected.
///
/// There is deliberately NO `From<ModelSelfReportedConfidence> for
/// Confidence`, no `Into`, and no `as_i32` that returns something a
/// `Confidence` constructor accepts by accident. The only bridge is
/// [`evidence_confidence_basis_points`], which does not read this value at
/// all. Adding a conversion here re-opens the defect.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ModelSelfReportedConfidence(u16);

impl ModelSelfReportedConfidence {
    /// Parses the raw column value, rejecting anything outside `0..=10000`.
    ///
    /// # Errors
    /// [`OutcomeValidationError::ConfidenceOutOfRange`] when out of range.
    pub fn parse(basis_points: i32) -> Result<Self, OutcomeValidationError> {
        u16::try_from(basis_points)
            .ok()
            .filter(|value| *value <= 10_000)
            .map(Self)
            .ok_or(OutcomeValidationError::ConfidenceOutOfRange(basis_points))
    }

    /// The raw self-report, for storage and display ONLY.
    ///
    /// Every caller must present it as the model's own claim. It must never
    /// be bound to a column, or passed to a constructor, whose meaning is
    /// evidence confidence.
    #[must_use]
    pub const fn self_reported_basis_points(self) -> u16 {
        self.0
    }
}

/// The evidence confidence the autopilot may attribute to an agent proposal.
///
/// **Not derived from the outcome.** The parameter is taken to keep the call
/// site honest about what is being converted, and is deliberately unused: an
/// LLM proposal carries no measured evidence confidence, so there is nothing
/// to derive one from and inventing a varying number would be worse than a
/// constant.
///
/// Why not zero: `autopilot/actions.rs` sweeps `awaiting_approval` actions
/// whose decision has `confidence_basis_points = 0` and cancels them as
/// `insufficient_evidence`. Zero is already a load-bearing value meaning "the
/// evidence check found nothing", and binding it here would cancel every
/// admitted agent proposal for the wrong reason.
///
/// Why the smallest non-zero value: decision confidence is the fifth and last
/// tiebreaker in `rank_next_best_actions` — never a filter, never an autonomy
/// gate for these kinds, whose disposition is hardcoded `require_approval` or
/// `recommend_only`. One basis point places an unmeasured proposal below
/// every measured finding on that tiebreak, which is the correct default, and
/// makes all agent proposals tie with each other, which is honest: nothing
/// here distinguishes their evidential strength.
#[must_use]
pub const fn evidence_confidence_basis_points(_outcome: &ValidatedOutcome) -> i32 {
    AGENT_PROPOSAL_EVIDENCE_BASIS_POINTS
}

/// See [`evidence_confidence_basis_points`]. Non-zero by necessity, minimal
/// by intent: "admitted, evidence confidence not measured".
pub const AGENT_PROPOSAL_EVIDENCE_BASIS_POINTS: i32 = 1;

/// A validated outcome ready to be mapped into autopilot rows.
#[derive(Clone, Debug)]
pub struct ValidatedOutcome {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub task_id: Uuid,
    pub result_id: Uuid,
    pub kind: OutcomeKind,
    pub schema_version: i32,
    pub payload: OutcomePayload,
    /// The model's claim about itself. See [`ModelSelfReportedConfidence`].
    pub self_reported_confidence: ModelSelfReportedConfidence,
    pub idempotency_key: String,
    /// Trace id propagated from the agents service (written to
    /// `agent_outcomes.trace_id`). When present, it links the autopilot
    /// decision/action created here back to the original brain trace that
    /// dispatched the agent worker. None for legacy rows predating the
    /// trace spine.
    pub trace_id: Option<Uuid>,
}

/// Why an outcome was rejected. Stored in `rejection_reason` for auditability;
/// the row is never deleted.
#[derive(Debug, thiserror::Error)]
pub enum OutcomeValidationError {
    #[error("unknown schema_version {0}; only version 1 is supported")]
    UnknownSchemaVersion(i32),
    #[error("unknown kind '{0}'")]
    UnknownKind(String),
    #[error("confidence_basis_points {0} out of range 0..=10000")]
    ConfidenceOutOfRange(i32),
    #[error("payload is not a JSON object")]
    PayloadNotObject,
    #[error("payload failed to deserialize: {0}")]
    Deserialize(#[from] serde_json::Error),
}

/// Validates a raw row read from `agent_outcomes`. The worker passes the
/// columns it just read; this function is pure and side-effect-free so it can
/// be unit-tested without a database.
///
/// # Errors
/// - [`OutcomeValidationError::UnknownSchemaVersion`] if `schema_version != 1`.
/// - [`OutcomeValidationError::UnknownKind`] if `kind` is not one of the eight.
/// - [`OutcomeValidationError::ConfidenceOutOfRange`] if outside `0..=10000`.
/// - [`OutcomeValidationError::PayloadNotObject`] if `payload` is not an object.
/// - [`OutcomeValidationError::Deserialize`] if `payload` does not match
///   [`OutcomePayload`].
#[allow(clippy::too_many_arguments)]
pub fn validate(
    id: Uuid,
    workspace_id: Uuid,
    task_id: Uuid,
    result_id: Uuid,
    kind: &str,
    schema_version: i32,
    payload: &serde_json::Value,
    confidence_basis_points: i32,
    idempotency_key: String,
    trace_id: Option<Uuid>,
) -> Result<ValidatedOutcome, OutcomeValidationError> {
    if schema_version != 1 {
        return Err(OutcomeValidationError::UnknownSchemaVersion(schema_version));
    }
    let kind = match kind {
        "press_pitch" => OutcomeKind::PressPitch,
        "social_post" => OutcomeKind::SocialPost,
        "signal_push" => OutcomeKind::SignalPush,
        "audience_segments" => OutcomeKind::AudienceSegments,
        "outreach_targets" => OutcomeKind::OutreachTargets,
        "campaign_insight" => OutcomeKind::CampaignInsight,
        "release_plan_note" => OutcomeKind::ReleasePlanNote,
        "generic_insight" => OutcomeKind::GenericInsight,
        other => return Err(OutcomeValidationError::UnknownKind(other.to_owned())),
    };
    let self_reported_confidence = ModelSelfReportedConfidence::parse(confidence_basis_points)?;
    if !payload.is_object() {
        return Err(OutcomeValidationError::PayloadNotObject);
    }
    let payload: OutcomePayload = serde_json::from_value(payload.clone())?;
    Ok(ValidatedOutcome {
        id,
        workspace_id,
        task_id,
        result_id,
        kind,
        schema_version,
        payload,
        self_reported_confidence,
        idempotency_key,
        trace_id,
    })
}

/// Why an outcome's provenance bars it from becoming an action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProvenanceRejection {
    /// No provenance on the row at all — nothing recorded what this was
    /// built from, so nothing can vouch for it.
    Missing,
    /// The grounding check did not pass: it was rejected, or never ran.
    NotGroundingChecked(VerificationStatus),
    /// A data source the run needed did not complete. The output may still
    /// be a fine draft, but it is not fit to become a pending action.
    DegradedContext,
}

impl std::fmt::Display for ProvenanceRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(
                f,
                "MISSING_PROVENANCE: the row records nothing about how it was produced"
            ),
            Self::NotGroundingChecked(status) => write!(
                f,
                "NOT_GROUNDING_CHECKED: verification status is {status:?}, not GroundingCheckPassed"
            ),
            Self::DegradedContext => write!(
                f,
                "DEGRADED_CONTEXT: a data source did not complete, so an absence in this output proves nothing"
            ),
        }
    }
}

/// Provenance gate for outcomes that become pending actions.
///
/// Applies to `require_approval` kinds only. Those reach fans, journalists and
/// communities the moment an operator clicks approve, and the click is the
/// only thing between the model and the outside world — so the row behind it
/// must at least have been grounding-checked against a context that actually
/// loaded. `recommend_only` kinds are observations shown on a board; gating
/// them would delete intelligence instead of labelling it, and their epistemic
/// status travels with them into the recycling path instead.
///
/// PASSING THIS GATE DOES NOT MAKE THE CLAIM TRUE. It means a second model
/// found no unsupported assertion in a context that fully loaded. That is a
/// bar for admission, not a finding of fact, and nothing downstream may treat
/// it as evidence that the world is as the outcome describes.
///
/// # Errors
/// [`ProvenanceRejection`] naming which of the three rules failed.
pub fn provenance_admission(
    kind: OutcomeKind,
    provenance: Option<&OutcomeProvenance>,
) -> Result<(), ProvenanceRejection> {
    if kind.disposition() != "require_approval" {
        return Ok(());
    }
    let Some(provenance) = provenance else {
        return Err(ProvenanceRejection::Missing);
    };
    if provenance.verification.status != VerificationStatus::GroundingCheckPassed {
        return Err(ProvenanceRejection::NotGroundingChecked(
            provenance.verification.status,
        ));
    }
    if provenance.context.any_source_failed {
        return Err(ProvenanceRejection::DegradedContext);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(rationale: &str) -> serde_json::Value {
        serde_json::json!({ "rationale": rationale })
    }

    #[test]
    fn validates_known_kind_v1() {
        let id = Uuid::now_v7();
        let ws = Uuid::now_v7();
        let task = Uuid::now_v7();
        let result = Uuid::now_v7();
        let outcome = validate(
            id,
            ws,
            task,
            result,
            "press_pitch",
            1,
            &payload("because"),
            7500,
            "agent:task:0".to_owned(),
            None,
        )
        .expect("valid outcome");
        assert_eq!(outcome.kind, OutcomeKind::PressPitch);
        assert_eq!(outcome.kind.disposition(), "require_approval");
        assert_eq!(outcome.kind.decision_kind(), "agent_content_proposal");
        assert_eq!(outcome.kind.autopilot_context(), "promotion_budget");
        assert_eq!(outcome.payload.rationale, "because");
    }

    #[test]
    fn rejects_unknown_schema_version() {
        let err = validate(
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            "press_pitch",
            2,
            &payload("x"),
            100,
            "k".to_owned(),
            None,
        )
        .expect_err("v2 rejected");
        assert!(matches!(
            err,
            OutcomeValidationError::UnknownSchemaVersion(2)
        ));
    }

    #[test]
    fn rejects_unknown_kind() {
        let err = validate(
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            "unknown_kind",
            1,
            &payload("x"),
            100,
            "k".to_owned(),
            None,
        )
        .expect_err("unknown kind rejected");
        assert!(matches!(err, OutcomeValidationError::UnknownKind(_)));
    }

    #[test]
    fn rejects_confidence_out_of_range() {
        let err = validate(
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            "press_pitch",
            1,
            &payload("x"),
            10001,
            "k".to_owned(),
            None,
        )
        .expect_err("confidence rejected");
        assert!(matches!(
            err,
            OutcomeValidationError::ConfidenceOutOfRange(10001)
        ));
    }

    #[test]
    fn recommend_only_kinds_have_no_action() {
        for kind in [
            "campaign_insight",
            "generic_insight",
            "release_plan_note",
            "audience_segments",
        ] {
            let outcome = validate(
                Uuid::now_v7(),
                Uuid::now_v7(),
                Uuid::now_v7(),
                Uuid::now_v7(),
                kind,
                1,
                &payload("x"),
                100,
                "k".to_owned(),
                None,
            )
            .expect("valid");
            assert_eq!(outcome.kind.disposition(), "recommend_only");
        }
    }

    fn provenance_json(status: &str, any_source_failed: bool) -> serde_json::Value {
        serde_json::json!({
            "rationale": "because",
            "provenance": {
                "confidence": { "basis_points": 9500, "source": "model_self_report", "is_evidence_confidence": false },
                "verification": { "check": "grounding_in_supplied_context", "status": status, "verifier_model": "v", "establishes_factual_correctness": false },
                "context": { "budget_chars": 100, "used_chars": 10, "any_source_failed": any_source_failed, "any_source_truncated": false, "sources": [] },
                "model": { "requested": "a", "actual": "b", "provider": "groq", "attempts": 2, "rejected_attempts": 1 }
            }
        })
    }

    fn outcome_with(kind: &str, payload: &serde_json::Value, confidence: i32) -> ValidatedOutcome {
        validate(
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            kind,
            1,
            payload,
            confidence,
            "k".to_owned(),
            None,
        )
        .expect("valid outcome")
    }

    // -- confidence type separation -----------------------------------------

    #[test]
    fn a_high_self_report_does_not_become_high_evidence_confidence() {
        // The whole defect in one assertion: the model claims 95% and the
        // autopilot decision must not inherit it.
        let outcome = outcome_with(
            "press_pitch",
            &provenance_json("grounding_check_passed", false),
            9_500,
        );
        assert_eq!(
            outcome
                .self_reported_confidence
                .self_reported_basis_points(),
            9_500
        );
        assert_eq!(evidence_confidence_basis_points(&outcome), 1);
        assert_ne!(
            i32::from(
                outcome
                    .self_reported_confidence
                    .self_reported_basis_points()
            ),
            evidence_confidence_basis_points(&outcome),
        );
    }

    #[test]
    fn evidence_confidence_ignores_the_self_report_entirely() {
        for claimed in [0, 1, 4_999, 9_500, 10_000] {
            let outcome = outcome_with(
                "press_pitch",
                &provenance_json("grounding_check_passed", false),
                claimed,
            );
            assert_eq!(
                evidence_confidence_basis_points(&outcome),
                AGENT_PROPOSAL_EVIDENCE_BASIS_POINTS,
                "self-report {claimed} must not move evidence confidence",
            );
        }
    }

    #[test]
    fn evidence_confidence_is_never_zero() {
        // Zero is the `insufficient_evidence` cancellation sweep's value in
        // autopilot/actions.rs. Binding it would cancel every admitted agent
        // proposal for a reason that never applied to it.
        assert_ne!(AGENT_PROPOSAL_EVIDENCE_BASIS_POINTS, 0);
        assert!((1..=10_000).contains(&AGENT_PROPOSAL_EVIDENCE_BASIS_POINTS));
    }

    #[test]
    fn self_report_rejects_out_of_range() {
        assert!(ModelSelfReportedConfidence::parse(-1).is_err());
        assert!(ModelSelfReportedConfidence::parse(10_001).is_err());
        assert!(ModelSelfReportedConfidence::parse(10_000).is_ok());
    }

    // -- provenance survives the handoff ------------------------------------

    #[test]
    fn provenance_deserializes_from_the_agents_payload() {
        let outcome = outcome_with(
            "press_pitch",
            &provenance_json("grounding_check_passed", false),
            9_500,
        );
        let provenance = outcome.payload.provenance.expect("provenance survived");
        assert_eq!(
            provenance.verification.status,
            VerificationStatus::GroundingCheckPassed
        );
        assert!(!provenance.context.any_source_failed);
        assert_eq!(
            provenance.confidence.source,
            ConfidenceSource::ModelSelfReport
        );
        assert_eq!(provenance.model.actual, "b");
        assert_eq!(provenance.model.provider.as_deref(), Some("groq"));
    }

    #[test]
    fn an_unknown_verification_status_reads_as_not_verified() {
        let outcome = outcome_with(
            "press_pitch",
            &provenance_json("grounding_check_passed_probably", false),
            5_000,
        );
        let provenance = outcome.payload.provenance.expect("provenance present");
        assert_eq!(
            provenance.verification.status,
            VerificationStatus::NotVerified,
            "a status this build cannot read must not be read as a pass",
        );
    }

    #[test]
    fn a_provenance_block_missing_its_context_reads_as_degraded() {
        let payload = serde_json::json!({
            "rationale": "x",
            "provenance": { "verification": { "status": "grounding_check_passed" } }
        });
        let outcome = outcome_with("press_pitch", &payload, 5_000);
        let provenance = outcome.payload.provenance.expect("provenance present");
        assert!(
            provenance.context.any_source_failed,
            "an unrecorded context must not be read as a healthy one",
        );
    }

    // -- fail-closed admission ----------------------------------------------

    #[test]
    fn an_actionable_outcome_without_provenance_is_not_admitted() {
        let outcome = outcome_with("press_pitch", &payload("no provenance here"), 9_500);
        assert_eq!(
            provenance_admission(outcome.kind, outcome.payload.provenance.as_ref()),
            Err(ProvenanceRejection::Missing),
        );
    }

    #[test]
    fn an_unverified_actionable_outcome_is_not_admitted() {
        for status in ["not_verified", "grounding_check_rejected"] {
            let outcome = outcome_with("social_post", &provenance_json(status, false), 9_500);
            assert!(
                matches!(
                    provenance_admission(outcome.kind, outcome.payload.provenance.as_ref()),
                    Err(ProvenanceRejection::NotGroundingChecked(_)),
                ),
                "{status} must not become a pending action",
            );
        }
    }

    #[test]
    fn a_degraded_context_bars_an_actionable_outcome() {
        let outcome = outcome_with(
            "signal_push",
            &provenance_json("grounding_check_passed", true),
            9_500,
        );
        assert_eq!(
            provenance_admission(outcome.kind, outcome.payload.provenance.as_ref()),
            Err(ProvenanceRejection::DegradedContext),
            "a connector that failed must not produce a fan-facing action",
        );
    }

    #[test]
    fn a_grounded_actionable_outcome_on_a_healthy_context_is_admitted() {
        for kind in [
            "press_pitch",
            "social_post",
            "signal_push",
            "outreach_targets",
        ] {
            let outcome = outcome_with(kind, &provenance_json("grounding_check_passed", false), 1);
            assert_eq!(
                provenance_admission(outcome.kind, outcome.payload.provenance.as_ref()),
                Ok(()),
                "{kind} with a clean grounding check and a healthy context is admissible",
            );
        }
    }

    #[test]
    fn observations_are_labelled_not_gated() {
        // recommend_only kinds are shown on a board, not sent to anyone.
        // Rejecting them would delete intelligence; their epistemic status
        // travels with them into the recycling path instead.
        for kind in [
            "campaign_insight",
            "generic_insight",
            "release_plan_note",
            "audience_segments",
        ] {
            let outcome = outcome_with(kind, &payload("no provenance"), 0);
            assert_eq!(
                provenance_admission(outcome.kind, outcome.payload.provenance.as_ref()),
                Ok(()),
                "{kind} is an observation, not an act",
            );
        }
    }

    #[test]
    fn validates_signal_push_kind() {
        let outcome = validate(
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            "signal_push",
            1,
            &payload("push notification draft"),
            8000,
            "k".to_owned(),
            None,
        )
        .expect("valid signal_push outcome");
        assert_eq!(outcome.kind, OutcomeKind::SignalPush);
        assert_eq!(outcome.kind.as_str(), "signal_push");
        assert_eq!(outcome.kind.disposition(), "require_approval");
        assert_eq!(outcome.kind.decision_kind(), "agent_signal_push_proposal");
        assert_eq!(outcome.kind.autopilot_context(), "fan_lifecycle");
    }
}
