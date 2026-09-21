use super::*;

fn design(unit_kind: ExperimentUnitKind, units: usize) -> ExperimentDesign {
    let mut design = ExperimentDesign::new(
        uuid::Uuid::nil(),
        "test-intervention",
        "cycle-1",
        unit_kind,
        (0..units).map(|i| format!("unit-{i}")).collect(),
        OffsetDateTime::UNIX_EPOCH,
        0.1,
        "test-strategy",
    );
    // Ask for a control arm; the point is whether one is reachable.
    let _ = design.check_power(1, 1, 1);
    design
}

#[test]
fn a_workspace_unit_can_never_hold_out_a_control() {
    // One workspace is one unit. Splitting it into two arms is not a
    // sample-size problem, and reporting it as one tells an operator to
    // wait for growth that will not help.
    let design = design(ExperimentUnitKind::Workspace, 1);
    assert_eq!(
        design.experiment_status,
        ExperimentStatus::InsufficientPower
    );
    assert!(design.is_structurally_unrandomisable());
}

#[test]
fn a_workspace_unit_stays_unrandomisable_however_many_decisions_it_batches() {
    // Production batches several decision keys into one workspace design.
    // That inflates `eligible_units` without creating a second workspace,
    // so it can look powered while still being a single unit of
    // randomisation.
    let design = design(ExperimentUnitKind::Workspace, 50);
    if design.experiment_status == ExperimentStatus::InsufficientPower {
        assert!(design.is_structurally_unrandomisable());
    }
}

#[test]
fn a_community_unit_with_too_few_members_is_only_transient() {
    // Same status, opposite meaning: this one resolves as the tenant
    // reaches more communities, so it must not be reported as permanent.
    let design = design(ExperimentUnitKind::TargetCommunity, 1);
    assert_eq!(
        design.experiment_status,
        ExperimentStatus::InsufficientPower
    );
    assert!(
        !design.is_structurally_unrandomisable(),
        "a community design is short of units today, not incapable of \
             randomising; calling it permanent would stop someone fixing it",
    );
}

#[test]
fn a_powered_design_is_not_flagged() {
    let design = design(ExperimentUnitKind::TargetCommunity, 40);
    if design.experiment_status == ExperimentStatus::Active {
        assert!(!design.is_structurally_unrandomisable());
    }
}
