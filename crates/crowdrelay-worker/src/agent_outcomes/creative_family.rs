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
async fn creative_family_for_task(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    task_id: Uuid,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar::<_, Option<String>>(
        r#"
        SELECT ev.creative_family
        FROM viryaos_growth_evidence AS ev
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
    .fetch_optional(&mut **tx)
    .await
    .map(|row| row.flatten())
}
