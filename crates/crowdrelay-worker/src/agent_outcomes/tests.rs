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

// ---------------------------------------------------------------------------
// Model-written text never rides a workspace-wide "yes".
//
// On 2026-08-31 four push notifications promising fans a presale code and
// rehearsal footage that did not exist, and on 2026-09-03 an invented
// rehearsal story posted to a subreddit, went out approved as
// `policy:bounded_auto` — a setting, not a person. A standing grant is a
// person's decision about one named community and still carries.
// ---------------------------------------------------------------------------

#[test]
fn a_workspace_policy_cannot_release_model_written_text() {
    assert_eq!(
        model_text_authority(UnattendedAuthority::Policy),
        UnattendedAuthority::Denied
    );
}

#[test]
fn a_persons_standing_grant_still_releases_its_one_community() {
    assert_eq!(
        model_text_authority(UnattendedAuthority::Grant),
        UnattendedAuthority::Grant
    );
    assert_eq!(
        model_text_authority(UnattendedAuthority::Denied),
        UnattendedAuthority::Denied
    );
}

#[test]
fn the_ingest_path_routes_community_posts_and_pushes_through_the_model_text_rule() {
    // A source check, because the rule only protects anything while the
    // authority decision actually calls it — and the channel flag must not
    // come back as a way around it.
    let source = include_str!("../agent_outcomes.rs");
    assert!(
        source.contains("model_text_authority(\n                        self.may_auto_execute(")
    );
    assert!(!source.contains("let authority = if channel_pre_approved"));
}

// ---------------------------------------------------------------------------
// A pitch goes to the target it was written for, or nowhere.
// ---------------------------------------------------------------------------

#[test]
fn a_pitch_names_its_one_addressee() {
    let id = Uuid::now_v7();
    let draft = serde_json::json!({ "target_refs": [id.to_string()] });
    assert_eq!(single_target_ref(Some(&draft)), Some(id));
    // The same id twice is still one addressee.
    let twice = serde_json::json!({ "target_refs": [id.to_string(), format!(" {id} ")] });
    assert_eq!(single_target_ref(Some(&twice)), Some(id));
}

#[test]
fn a_pitch_with_no_several_or_malformed_refs_has_no_recipient() {
    assert_eq!(single_target_ref(None), None);
    assert_eq!(single_target_ref(Some(&serde_json::json!({}))), None);
    assert_eq!(
        single_target_ref(Some(&serde_json::json!({ "target_refs": [] }))),
        None
    );
    assert_eq!(
        single_target_ref(Some(
            &serde_json::json!({ "target_refs": ["Metal Devastation Radio"] })
        )),
        None
    );
    let two = serde_json::json!({ "target_refs": [Uuid::now_v7().to_string(), Uuid::now_v7().to_string()] });
    assert_eq!(single_target_ref(Some(&two)), None);
}
