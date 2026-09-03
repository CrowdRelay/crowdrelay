// Fixture helpers for `autopilot_experiment_integrity_postgres.rs`,
// `include!`d into it the way `acquisition_postgres/helpers.rs` is, so they
// share the parent's scope and imports. Split out to keep the suite file
// under the source-size ratchet while the quarantined tests are repaired.

/// A `payload` the repository can actually deserialize for a given kind.
///
/// The column `action_kind` and the payload's `kind` tag are two different
/// vocabularies for the same action — `community.engage.request` pairs with
/// `request_community_engagement`, as `agent_outcomes.rs` writes them. The
/// fixture used the `action_kind` string as the payload tag as well, so
/// `record_execution_report` — which parses this payload on a success report —
/// failed with `unknown variant`, surfaced as a bare `Err(Unexpected)`.
fn payload_for_action_kind(action_kind: &str) -> serde_json::Value {
    match action_kind {
        "signal.push.request" => serde_json::json!({
            "kind": "request_signal_push",
            "task_id": uuid::Uuid::now_v7(),
            "title": "test push",
            "body": "test push body",
            "target_path": null,
            "event_id": null,
            "segment": null,
        }),
        _ => serde_json::json!({
            "kind": "request_community_engagement",
            "target_id": uuid::Uuid::now_v7(),
            "platform": "reddit",
            "subreddit": "r/test",
            "title": "test title",
            "body": "test body",
            "smart_link": null,
        }),
    }
}

/// Helper: insert a minimal autopilot decision + action for test setup.
///
/// Fixed to `community.engage.request`, which decides how
/// `viryaos_action_ledger_reconcile` resolves the action: that kind takes the
/// `community_posts` strategy and never reads the outbox. Tests about outbox
/// reconciliation must use [`insert_decision_and_action_with_kind`] instead.
async fn insert_decision_and_action(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    trace_id: uuid::Uuid,
) -> uuid::Uuid {
    insert_decision_and_action_with_kind(pool, workspace_id, trace_id, "community.engage.request")
        .await
}

/// Helper: the same fixture with the action kind chosen by the caller.
///
/// `action_kind` is not decoration. `viryaos_action_ledger_reconcile` branches
/// on it: `community.engage.request` resolves through `community_posts`, and
/// every other kind resolves through the delivery status of the action's
/// newest `outbox_events` row. The outbox tests inserted a delivered or dead
/// outbox event and then called reconcile on a community action, so the
/// function took the community branch, found no post row and returned UNKNOWN
/// without ever reading the event. That failed the two tests expecting a
/// resolution, and — worse — silently passed the one expecting UNKNOWN, which
/// had been green for the wrong reason.
async fn insert_decision_and_action_with_kind(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    trace_id: uuid::Uuid,
    action_kind: &str,
) -> uuid::Uuid {
    let decision_id = uuid::Uuid::now_v7();
    let action_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_decisions
           (id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id)
           VALUES ($1,$2,$3,'growth_metrics','target_community',$4,
                   'auto_execute',9000,'auto_execute','test',
                   '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,$5)"#,
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("decision-{decision_id}"))
    .bind(uuid::Uuid::now_v7())
    .bind(trace_id)
    .execute(pool)
    .await
    .expect("insert decision");

    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, approved_at, available_at,
            finished_at, trace_id)
           VALUES ($1,$2,$3,'growth_metrics',$8,'target_community',
                   $4,$5,$6,'succeeded',now(),now(),now(),$7)"#,
    )
    .bind(action_id)
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(uuid::Uuid::now_v7())
    .bind(format!("action-{action_id}"))
    .bind(payload_for_action_kind(action_kind))
    .bind(trace_id)
    .bind(action_kind)
    .execute(pool)
    .await
    .expect("insert action");
    action_id
}

/// Helper: insert a community_posts row for a given action.
async fn insert_community_post(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    action_id: uuid::Uuid,
    status: &str,
    subreddit: &str,
) -> uuid::Uuid {
    let post_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO community_posts
           (id, workspace_id, action_id, subreddit, title, body, smart_link, status)
           VALUES ($1,$2,$3,$4,'Test post','Test body',NULL,$5)"#,
    )
    .bind(post_id)
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .bind(subreddit)
    .bind(status)
    .execute(pool)
    .await
    .expect("insert community_post");
    post_id
}

/// Helper: insert a community_posts row with an error_message.
async fn insert_community_post_with_error(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    action_id: uuid::Uuid,
    status: &str,
    subreddit: &str,
    error_message: &str,
) -> uuid::Uuid {
    let post_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO community_posts
           (id, workspace_id, action_id, subreddit, title, body, smart_link, status, error_message)
           VALUES ($1,$2,$3,$4,'Test post','Test body',NULL,$5,$6)"#,
    )
    .bind(post_id)
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .bind(subreddit)
    .bind(status)
    .bind(error_message)
    .execute(pool)
    .await
    .expect("insert community_post with error");
    post_id
}

/// Helper: insert a bare experiment assignment with a specific action_id.
async fn insert_bare_assignment(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    action_id: uuid::Uuid,
    experiment_uuid: uuid::Uuid,
    unit_id: &str,
) {
    insert_experiment_design(pool, workspace_id, experiment_uuid).await;
    insert_bare_action(pool, workspace_id, action_id).await;
    sqlx::query(
        r#"INSERT INTO viryaos_experiment_assignments
           (id, workspace_id, experiment_uuid, unit_id, unit_kind,
            arm, intended_template_id, propensity, prediction, context, strategy,
            eligibility_criteria, selection_context, interference_policy,
            contamination_estimate, is_interference_controllable,
            experiment_status, execution_status, action_id)
           VALUES ($1,$2,$3,$4,'target_community','treatment','reddit-scanner',0.5,
                   '{}'::jsonb,'{}'::jsonb,'discovery',
                   '{}'::jsonb,'{}'::jsonb,'none',0.0,false,
                   'active','dispatched',$5)"#,
    )
    .bind(uuid::Uuid::now_v7().to_string())
    .bind(workspace_id.into_uuid())
    .bind(experiment_uuid)
    .bind(unit_id)
    .bind(action_id)
    .execute(pool)
    .await
    .expect("insert bare assignment");
}

/// Inserts the decision and action an assignment's `action_id` points at.
///
/// `viryaos_experiment_assignments_action_id_fkey` requires the action to
/// exist. Fixtures used to mint a bare uuid, which was fine before the key
/// existed and has been failing ever since — production never assigns a unit to
/// an action it did not create.
async fn insert_bare_action(pool: &sqlx::PgPool, workspace_id: WorkspaceId, action_id: uuid::Uuid) {
    let decision_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_decisions
             (id, workspace_id, decision_key, context, subject_kind, subject_id,
              decision_kind, confidence_basis_points, disposition, reason,
              input_snapshot, policy_snapshot, recommendation)
           VALUES ($1,$2,$3,'growth_intelligence','target_community',$4,
                   'request_agent_run',5000,'require_approval','fixture',
                   '{}'::jsonb,'{}'::jsonb,'{}'::jsonb)"#,
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("fixture:{action_id}"))
    .bind(uuid::Uuid::now_v7())
    .execute(pool)
    .await
    .expect("insert fixture decision");
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_actions
             (id, workspace_id, decision_id, context, action_kind,
              subject_kind, subject_id, idempotency_key, payload, status)
           VALUES ($1,$2,$3,'growth_intelligence','request_agent_run',
                   'target_community',$4,$5,'{}'::jsonb,'queued')"#,
    )
    .bind(action_id)
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(uuid::Uuid::now_v7())
    .bind(format!("fixture-action:{action_id}"))
    .execute(pool)
    .await
    .expect("insert fixture action");
}

/// Inserts the `viryaos_experiment_designs` parent row an assignment's
/// `experiment_uuid` foreign key points at.
///
/// Fixtures used to mint an `experiment_uuid` and insert the assignment
/// directly, which `fk_assignment_experiment` rejects. Production always
/// designs an experiment before assigning units to it, so the fixture has to
/// do the same to exercise anything downstream.
async fn insert_experiment_design(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    experiment_uuid: uuid::Uuid,
) {
    sqlx::query(
        r#"INSERT INTO viryaos_experiment_designs
             (experiment_uuid, workspace_id, intervention_key, logical_cycle_key,
              unit_kind, holdout_probability, interference_policy, experiment_status)
           VALUES ($1, $2, 'reddit-scanner', $3, 'target_community', 0.5, 'none', 'active')
           ON CONFLICT (experiment_uuid) DO NOTHING"#,
    )
    .bind(experiment_uuid)
    .bind(workspace_id.into_uuid())
    .bind(experiment_uuid.to_string())
    .execute(pool)
    .await
    .expect("insert experiment design");
}
