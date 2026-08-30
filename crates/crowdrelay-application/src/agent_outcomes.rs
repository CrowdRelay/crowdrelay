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

/// The envelope row as written by the agents service. The worker reads
/// `payload` as a JSONB object and extracts `rationale` + optional `item`
/// from it; the envelope-level row (no items) stores `{ rationale, kind }`,
/// item rows store `{ item, rationale }`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OutcomePayload {
    #[serde(default)]
    pub rationale: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<serde_json::Value>,
}

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
    pub confidence_basis_points: i32,
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
    if !(0..=10000).contains(&confidence_basis_points) {
        return Err(OutcomeValidationError::ConfidenceOutOfRange(
            confidence_basis_points,
        ));
    }
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
        confidence_basis_points,
        idempotency_key,
        trace_id,
    })
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
