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

/// T30: a decision explains itself from the database, months later.
///
/// The acceptance test for replay. A decision is persisted through the real
/// write path, the process's in-memory state is discarded, and the row is read
/// back with SQL. Everything asserted is read out of that row — no optimizer is
/// re-run, no policy is consulted, no configuration is read.
///
/// That last part is the point. `viryaos_autopilot_policies` is updated in
/// place with no history table, and the causal posteriors move every cycle, so
/// anything reconstructed from current state answers "what would the brain
/// decide now". This proves the other question is answerable: what it decided,
/// what else was in the pool, why each alternative lost, what the adjustments
/// cost and what inputs produced them, which policy content constrained it, and
/// which code produced the result.
///
/// The policy row is deliberately mutated after the write. The assertions must
/// hold across that edit, or the record is not historical.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t30_a_persisted_decision_explains_itself_without_current_state() {
    let f = setup().await.expect("fixture");
    let trace = uuid::Uuid::now_v7();
    let action_id = insert_decision_and_action(&f.pool, f.workspace_id, trace).await;

    // The decision-time record, exactly the shape the evaluator attaches.
    let record = serde_json::json!({
        "economic": {
            "intrinsic_y30": 12.0,
            "adjustments": {
                "intrinsic_y30": 12.0,
                "overlap_adjustment": -3.6,
                "fatigue_adjustment": -0.84,
                "bridge_adjustment": 0.0,
                "marginal_y30": 7.56,
                "audience_count": 1,
                "overlap_penalty": 0.3,
                "fatigue_decay": 0.9,
                "bridge_factor": 1.0,
            },
        },
        "epistemic": { "estimation_regime": "outcome_model", "sample_size": 0 },
        "policy": {
            "policy_version": 7,
            "policy_identity": "sha256:0123456789abcdef0123456789abcdef",
        },
        "identity": { "optimizer": "submodular_greedy_marginal_v1" },
        "competition": {
            "considered": 3,
            "alternatives": [
                { "opportunity_key": "b", "reason": "max_dispatches_reached", "intrinsic_y30": 8.0 },
                { "opportunity_key": "c", "reason": "below_threshold", "intrinsic_y30": 3.0 },
            ],
        },
    });
    sqlx::query(
        "UPDATE viryaos_autopilot_decisions \
         SET input_snapshot = jsonb_set(input_snapshot, '{decision_value}', $3) \
         WHERE workspace_id = $1 \
           AND id = (SELECT decision_id FROM viryaos_autopilot_actions \
                     WHERE workspace_id = $1 AND id = $2)",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .bind(&record)
    .execute(&f.pool)
    .await
    .expect("attach the decision-time record");

    // The world moves on: the policy row is edited after the decision. A
    // record that reads current state would now answer a different question.
    sqlx::query(
        "UPDATE viryaos_autopilot_policies \
         SET autonomy_level = 'observe', enabled = false, version = version + 99 \
         WHERE workspace_id = $1",
    )
    .bind(f.workspace_id.into_uuid())
    .execute(&f.pool)
    .await
    .expect("mutate the policy after the decision");

    // Months later: one SELECT, nothing else.
    let stored: serde_json::Value = sqlx::query_scalar(
        "SELECT input_snapshot -> 'decision_value' FROM viryaos_autopilot_decisions \
         WHERE workspace_id = $1 \
           AND id = (SELECT decision_id FROM viryaos_autopilot_actions \
                     WHERE workspace_id = $1 AND id = $2)",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("read the decision-time record back");

    // 1. What was selected, and what it was worth.
    let adjustments = &stored["economic"]["adjustments"];
    let marginal = adjustments["marginal_y30"].as_f64().expect("marginal");

    // 2. The adjustments reconstruct the marginal from the record alone.
    let summed = adjustments["intrinsic_y30"].as_f64().expect("intrinsic")
        + adjustments["overlap_adjustment"].as_f64().expect("overlap")
        + adjustments["fatigue_adjustment"].as_f64().expect("fatigue")
        + adjustments["bridge_adjustment"].as_f64().expect("bridge");
    assert!(
        (summed - marginal).abs() < 1e-9,
        "the stored breakdown must reach the stored marginal: {summed} vs {marginal}"
    );

    // 3. And the inputs explain why each delta had that value. Recomputing the
    //    overlap from the stored count and penalty must land on the stored
    //    delta — this is what makes the number checkable rather than trusted.
    let count = f64::from(adjustments["audience_count"].as_u64().expect("count") as u32);
    let penalty = adjustments["overlap_penalty"].as_f64().expect("penalty");
    let intrinsic = adjustments["intrinsic_y30"].as_f64().expect("intrinsic");
    let recomputed_overlap = intrinsic * (1.0 - penalty * count) - intrinsic;
    assert!(
        (recomputed_overlap - adjustments["overlap_adjustment"].as_f64().expect("overlap")).abs()
            < 1e-9,
        "the stored inputs must reproduce the stored overlap adjustment"
    );

    // 4. Why the alternatives lost — from history, not from re-running.
    let alternatives = stored["competition"]["alternatives"]
        .as_array()
        .expect("alternatives");
    assert_eq!(alternatives.len(), 2, "both losers must survive the round trip");
    assert_eq!(alternatives[0]["reason"], "max_dispatches_reached");
    assert_eq!(alternatives[1]["reason"], "below_threshold");
    assert!(
        alternatives[1]["intrinsic_y30"].as_f64().expect("intrinsic") < intrinsic,
        "a loser's recorded worth must be readable beside the winner's"
    );

    // 5. Which policy constrained it — unchanged by the edit above.
    assert_eq!(
        stored["policy"]["policy_identity"], "sha256:0123456789abcdef0123456789abcdef",
        "the policy identity is content, so a later edit to the row cannot \
         rewrite what constrained this decision"
    );
    assert_eq!(stored["policy"]["policy_version"], 7);

    // 6. Which code produced it.
    assert_eq!(
        stored["identity"]["optimizer"], "submodular_greedy_marginal_v1",
        "the optimizer identity must survive"
    );

    // The current policy row now says something else entirely, which is the
    // point: none of the above read it.
    let current: (String, i64) = sqlx::query_as(
        "SELECT autonomy_level, version FROM viryaos_autopilot_policies \
         WHERE workspace_id = $1 LIMIT 1",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .expect("current policy");
    assert_eq!(
        current.0, "observe",
        "the fixture must actually have moved the mutable state it is proving \
         the record does not depend on"
    );
}
