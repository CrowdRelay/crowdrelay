// Recovering the creative family a community-engagement task ran under.
//
// `include!`d into `agent_outcomes.rs` so it shares that module's scope.
// Split out to keep the parent inside the source-size ratchet.

/// The creative family the producing run's evidence row carries, keyed
/// through the task that dispatched it. The engager records its pick on the
/// run's evidence; the engage action it spawned is what actually publishes,
/// so the family has to cross from the run's evidence to the action's
/// payload for the post's own measurement to teach the family posterior.
/// `None` means the run predates family attribution or never recorded one —
/// the action publishes unmeasured for family rather than inheriting a
/// guess.
///
/// Runs on the pool, never inside the caller's transaction, for the same
/// reason `resolve_trace` does: `agent_service_tasks` is the agents
/// service's own schema and a deployment without it must still ingest
/// outcomes. A failed statement inside the transaction would poison the
/// whole mapping — and an erroring lookup is just another form of "no
/// family recorded", which `None` already means.
async fn creative_family_for_task(
    pool: &PgPool,
    outcome_id: Uuid,
    workspace_id: Uuid,
    task_id: Uuid,
) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>(
        r#"
        SELECT ev.creative_family
        FROM growth_evidence AS ev
        JOIN agent_service_tasks AS task
          ON task.workspace_id = ev.workspace_id
         AND (task.metadata->>'action_id')::uuid = ev.action_id
        WHERE task.workspace_id = $1 AND task.id = $2
          AND ev.creative_family IS NOT NULL
        ORDER BY ev.timestamp DESC
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .bind(task_id)
    .fetch_optional(pool)
    .await
    .unwrap_or_else(|error| {
        tracing::debug!(
            outcome_id = %outcome_id,
            error = %error,
            "could not resolve the creative family; the action goes out unmeasured for it"
        );
        None
    })
    .flatten()
}
