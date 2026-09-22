use super::*;
use crowdrelay_application::agent_outcomes::{
    ContextProvenance, ModelSelfReportedConfidence, OutcomeKind, OutcomePayload, OutcomeProvenance,
    VerificationProvenance, VerificationStatus,
};

/// Builds a valid provenance for actionable outcome kinds (those with
/// `require_approval` disposition). Observation kinds do not need it,
/// but including it does not change their admission.
fn valid_provenance() -> Option<OutcomeProvenance> {
    Some(OutcomeProvenance {
        verification: VerificationProvenance {
            status: VerificationStatus::GroundingCheckPassed,
        },
        context: ContextProvenance {
            any_source_failed: false,
            any_source_truncated: false,
        },
        ..Default::default()
    })
}

pub(super) fn make_outcome(
    kind: OutcomeKind,
    confidence: i32,
    item: Option<Value>,
) -> ValidatedOutcome {
    let needs_provenance = kind.disposition() == "require_approval";
    ValidatedOutcome {
        id: Uuid::now_v7(),
        workspace_id: Uuid::now_v7(),
        task_id: Uuid::now_v7(),
        result_id: Uuid::now_v7(),
        kind,
        schema_version: 1,
        payload: OutcomePayload {
            rationale: "test".to_owned(),
            item,
            provenance: if needs_provenance {
                valid_provenance()
            } else {
                None
            },
        },
        self_reported_confidence: ModelSelfReportedConfidence::parse(confidence)
            .expect("test confidence in range"),
        idempotency_key: "test-key".to_owned(),
        trace_id: None,
    }
}

#[test]
fn zero_confidence_outreach_target_is_rejected() {
    let outcome = make_outcome(
        OutcomeKind::OutreachTargets,
        0,
        Some(json!({
            "target_kind": "creator",
            "display_name": "r/metalpolska",
            "evidence_urls": ["https://reddit.com/r/metalpolska"],
        })),
    );
    let err = evaluate_outcome_quality(&outcome).expect_err("must reject");
    assert!(
        matches!(err, OutcomeRejection::InsufficientEvidence { .. }),
        "expected InsufficientEvidence, got {err:?}"
    );
}

#[test]
fn zero_confidence_press_pitch_is_rejected() {
    let outcome = make_outcome(
        OutcomeKind::PressPitch,
        0,
        Some(json!({"subject": "test", "body": "test"})),
    );
    let err = evaluate_outcome_quality(&outcome).expect_err("must reject");
    assert!(
        matches!(err, OutcomeRejection::InsufficientEvidence { .. }),
        "expected InsufficientEvidence, got {err:?}"
    );
}

#[test]
fn unnamed_target_is_rejected() {
    let outcome = make_outcome(
        OutcomeKind::OutreachTargets,
        5000,
        Some(json!({
            "target_kind": "creator",
            "display_name": "Unnamed target",
            "evidence_urls": ["https://reddit.com/r/test"],
        })),
    );
    let err = evaluate_outcome_quality(&outcome).expect_err("must reject");
    assert!(
        matches!(err, OutcomeRejection::MissingTargetIdentity),
        "expected MissingTargetIdentity, got {err:?}"
    );
}

#[test]
fn missing_display_name_is_rejected() {
    let outcome = make_outcome(
        OutcomeKind::OutreachTargets,
        5000,
        Some(json!({
            "target_kind": "creator",
            "evidence_urls": ["https://reddit.com/r/test"],
        })),
    );
    let err = evaluate_outcome_quality(&outcome).expect_err("must reject");
    assert!(
        matches!(err, OutcomeRejection::MissingTargetIdentity),
        "expected MissingTargetIdentity, got {err:?}"
    );
}

#[test]
fn empty_display_name_is_rejected() {
    let outcome = make_outcome(
        OutcomeKind::OutreachTargets,
        5000,
        Some(json!({
            "target_kind": "creator",
            "display_name": "  ",
            "evidence_urls": ["https://reddit.com/r/test"],
        })),
    );
    let err = evaluate_outcome_quality(&outcome).expect_err("must reject");
    assert!(
        matches!(err, OutcomeRejection::MissingTargetIdentity),
        "expected MissingTargetIdentity, got {err:?}"
    );
}

#[test]
fn empty_evidence_urls_is_rejected() {
    let outcome = make_outcome(
        OutcomeKind::OutreachTargets,
        5000,
        Some(json!({
            "target_kind": "creator",
            "display_name": "r/metalpolska",
            "evidence_urls": [],
        })),
    );
    let err = evaluate_outcome_quality(&outcome).expect_err("must reject");
    assert!(
        matches!(err, OutcomeRejection::InsufficientEvidence { .. }),
        "expected InsufficientEvidence, got {err:?}"
    );
}

#[test]
fn missing_evidence_urls_is_rejected() {
    let outcome = make_outcome(
        OutcomeKind::OutreachTargets,
        5000,
        Some(json!({
            "target_kind": "creator",
            "display_name": "r/metalpolska",
        })),
    );
    let err = evaluate_outcome_quality(&outcome).expect_err("must reject");
    assert!(
        matches!(err, OutcomeRejection::InsufficientEvidence { .. }),
        "expected InsufficientEvidence, got {err:?}"
    );
}

#[test]
fn no_item_outreach_target_is_the_honest_empty() {
    // The emit side writes a single item-less row when the model's items
    // array came back empty — "looked, found nothing worth filing". That is
    // a valid observation, not a malformed one; rejecting it counts a
    // correctly-behaving task as a failure in the execution-health window.
    let outcome = make_outcome(OutcomeKind::OutreachTargets, 5000, None);
    evaluate_outcome_quality(&outcome).expect("honest empty must pass");
}

#[test]
fn valid_outreach_target_passes() {
    let outcome = make_outcome(
        OutcomeKind::OutreachTargets,
        5000,
        Some(json!({
            "target_kind": "creator",
            "display_name": "r/metalpolska",
            "evidence_urls": ["https://reddit.com/r/metalpolska"],
        })),
    );
    evaluate_outcome_quality(&outcome).expect("valid outcome must pass the guard");
}

#[test]
fn zero_confidence_insight_passes() {
    // Insights are recommend_only — confidence 0 is a weak observation,
    // not a dangerous action. The guard must not reject them.
    let outcome = make_outcome(
        OutcomeKind::GenericInsight,
        0,
        Some(json!({"headline": "test", "detail": "test"})),
    );
    evaluate_outcome_quality(&outcome).expect("insights must pass even at confidence 0");
}

#[test]
fn zero_confidence_audience_segments_passes() {
    let outcome = make_outcome(
        OutcomeKind::AudienceSegments,
        0,
        Some(json!({"name": "test", "description": "test"})),
    );
    evaluate_outcome_quality(&outcome).expect("segments must pass even at confidence 0");
}

#[test]
fn valid_press_pitch_passes() {
    let outcome = make_outcome(
        OutcomeKind::PressPitch,
        7500,
        Some(json!({"subject": "test", "body": "test"})),
    );
    evaluate_outcome_quality(&outcome).expect("valid press pitch must pass");
}

#[test]
fn valid_signal_push_passes() {
    let outcome = make_outcome(
        OutcomeKind::SignalPush,
        8000,
        Some(json!({"title": "test", "body": "test"})),
    );
    evaluate_outcome_quality(&outcome).expect("valid signal push must pass");
}
