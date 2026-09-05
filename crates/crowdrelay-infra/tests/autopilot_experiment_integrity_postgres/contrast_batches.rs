// Delta replay must reach a control arm that resolved before the checkpoint.
//
// `include!`d into `autopilot_experiment_integrity_postgres.rs`, so this shares
// the parent's scope and imports — `setup`, `insert_experiment_design` and
// `insert_decision_and_action` all come from there.

/// Helper: an experiment's control arm, resolved at a chosen moment.
///
/// The control unit is never dispatched, so its evidence has no `action_id`
/// and the only key into the randomisation it carries is
/// `experiment_assignment_id`.
async fn insert_resolved_control_arm(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    experiment_uuid: uuid::Uuid,
    opportunity_id: &str,
    resolved_at: OffsetDateTime,
) {
    let assignment_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        r#"INSERT INTO viryaos_experiment_assignments
           (workspace_id, id, experiment_uuid, unit_id, unit_kind,
            arm, intended_template_id, propensity, prediction, context, strategy,
            eligibility_criteria, selection_context, interference_policy,
            contamination_estimate, is_interference_controllable,
            experiment_status, execution_status, action_id)
           VALUES ($1,$2,$3,'r/contrast-control','target_community','control',
                   'community-engager',0.5,'{}'::jsonb,'{}'::jsonb,'discovery',
                   '{}'::jsonb,'{}'::jsonb,'none',0.0,false,
                   'active','control',NULL)"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&assignment_id)
    .bind(experiment_uuid)
    .execute(pool)
    .await
    .expect("insert control assignment");

    sqlx::query(
        r#"INSERT INTO viryaos_growth_evidence
           (workspace_id, action_id, opportunity_id, timestamp, audience,
            recipient_id, channel, estimated_reach, treatment, propensity,
            observed_fans, observed_incremental_fans, durable_fans_30d,
            converted, predicted_fans, predicted_signal_installs, context,
            strategy, evidence_quality, experiment_assignment_id, resolved_at)
           VALUES ($1,NULL,$2,$5,'test','r/contrast-control','reddit_post',1,
                   'control',0.5,1.0,1.0,2.0,false,0.0,0.0,
                   '{}'::jsonb,'discovery','randomized_holdout',$3,$4)"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(opportunity_id)
    .bind(&assignment_id)
    .bind(resolved_at)
    .bind(resolved_at)
    .execute(pool)
    .await
    .expect("insert control evidence");
}

/// Helper: a treated unit of the same experiment, resolved at a chosen moment.
async fn insert_resolved_treated_unit(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    experiment_uuid: uuid::Uuid,
    action_id: uuid::Uuid,
    opportunity_id: &str,
    resolved_at: OffsetDateTime,
) {
    let assignment_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        r#"INSERT INTO viryaos_experiment_assignments
           (workspace_id, id, experiment_uuid, unit_id, unit_kind,
            arm, intended_template_id, propensity, prediction, context, strategy,
            eligibility_criteria, selection_context, interference_policy,
            contamination_estimate, is_interference_controllable,
            experiment_status, execution_status, action_id, final_contamination)
           VALUES ($1,$2,$3,'r/contrast-treated','target_community','treatment',
                   'community-engager',0.5,'{}'::jsonb,'{}'::jsonb,'discovery',
                   '{}'::jsonb,'{}'::jsonb,'none',0.0,false,
                   'active','executed',$4,0.0)"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&assignment_id)
    .bind(experiment_uuid)
    .bind(action_id)
    .execute(pool)
    .await
    .expect("insert treated assignment");

    sqlx::query(
        r#"INSERT INTO viryaos_growth_evidence
           (workspace_id, action_id, opportunity_id, timestamp, audience,
            recipient_id, channel, estimated_reach, treatment, propensity,
            observed_fans, observed_incremental_fans, durable_fans_30d,
            converted, predicted_fans, predicted_signal_installs, context,
            strategy, evidence_quality, experiment_assignment_id, resolved_at)
           VALUES ($1,$2,$3,$6,'test','r/contrast-treated','reddit_post',1,
                   'treatment',0.5,4.0,4.0,10.0,false,2.0,0.0,
                   '{}'::jsonb,'discovery','randomized_holdout',$4,$5)"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .bind(opportunity_id)
    .bind(&assignment_id)
    .bind(resolved_at)
    .bind(resolved_at)
    .execute(pool)
    .await
    .expect("insert treated evidence");
}

/// T29: delta replay reaches a control arm that resolved before the checkpoint.
///
/// The learning cursor is `resolved_at` and the randomisation does not respect
/// it. An experiment's treated units resolve when their own measurements
/// finish, which is not the same day for all of them; the control arm resolves
/// once, alongside the first. Every treated row after that arrived in a batch
/// containing no control row, was capped at quasi-experimental, and contributed
/// a raw pre/post difference. A randomised experiment degrading to an
/// observational one because of when a checkpoint happened to be taken.
///
/// The property, measured through the production path: **a checkpoint must not
/// change what the model learns.** Full replay sees the control arm because it
/// sees everything; delta replay has to go and fetch it. Both are run against
/// the same rows and must land on the same Y30 treatment effect.
///
/// This is also the only place the control-arm query runs against a real
/// schema. It binds a `uuid[]`, filters on `ge.treatment`, and reaches
/// `ea.experiment_uuid` through the LATERAL join — none of which a unit test
/// can check, and all of which would first fail on a live request.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t29_delta_replay_reaches_a_control_arm_resolved_before_the_checkpoint() {
    use crowdrelay_brain::CausalModel;

    let f = setup().await.expect("fixture");
    let now = OffsetDateTime::now_utc();
    let experiment_uuid = uuid::Uuid::now_v7();
    insert_experiment_design(&f.pool, f.workspace_id, experiment_uuid).await;

    // The control arm resolves first — it always does, because a control unit
    // has no measurement of its own and is swept when the first treated unit's
    // measurement completes.
    insert_resolved_control_arm(
        &f.pool,
        f.workspace_id,
        experiment_uuid,
        "community-engager:contrast:post:ctx",
        now - time::Duration::days(10),
    )
    .await;

    // A treated unit whose own measurement finished later.
    let trace = uuid::Uuid::now_v7();
    let action_id = insert_decision_and_action(&f.pool, f.workspace_id, trace).await;
    insert_resolved_treated_unit(
        &f.pool,
        f.workspace_id,
        experiment_uuid,
        action_id,
        "community-engager:contrast:post:ctx",
        now,
    )
    .await;

    let context = DispatchContext::default();
    let effect_of = |model: &CausalModel| {
        model
            .predict_stats_with_treatment("community-engager", &context)
            .treatment_effect_y30
    };

    // No checkpoint: full replay, control arm in the batch.
    let full = f
        .repository
        .load_causal_model(f.workspace_id)
        .await
        .expect("full replay");

    // Now a checkpoint of an untaught model, dated between the two
    // resolutions. `save_brain_state` stamps `updated_at` with the wall clock,
    // so it is moved back explicitly — the point of the test is a cursor that
    // sits after the control arm and before the treated row.
    f.repository
        .save_brain_state(
            f.workspace_id,
            "causal_model",
            &serde_json::to_value(CausalModel::new()).expect("serialize model"),
        )
        .await
        .expect("save checkpoint");
    sqlx::query(
        "UPDATE viryaos_brain_state SET updated_at = $2 \
         WHERE workspace_id = $1 AND module = 'causal_model'",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(now - time::Duration::days(5))
    .execute(&f.pool)
    .await
    .expect("backdate the checkpoint");

    let delta = f
        .repository
        .load_causal_model(f.workspace_id)
        .await
        .expect("delta replay");

    assert!(
        effect_of(&full).abs() > 1e-9,
        "the fixture must teach the Y30 posterior something, otherwise the \
         comparison below holds vacuously"
    );
    assert!(
        (effect_of(&full) - effect_of(&delta)).abs() < 1e-9,
        "a checkpoint must not change what the model learns. Full replay saw \
         the control arm and got {}; delta replay had to fetch it and got {}",
        effect_of(&full),
        effect_of(&delta)
    );
}
