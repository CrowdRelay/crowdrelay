use crowdrelay_application::{
    EcosystemControlPlaneRepository, EcosystemRepositoryError, RunReconciliationCommand,
    UpdateFeatureFlagCommand, UpdateShowChecklistCommand,
};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{database, ecosystem::PostgresEcosystemRepository};
use sqlx::postgres::PgPoolOptions;

const TEST_DATABASE_URL_KEY: &str = "CROWDRELAY_ECOSYSTEM_TEST_DATABASE_URL";

fn command(
    workspace_id: WorkspaceId,
    idempotency_key: &str,
    enabled: bool,
) -> UpdateFeatureFlagCommand {
    UpdateFeatureFlagCommand {
        workspace_id,
        key: "ticket_sales_enabled".to_owned(),
        enabled,
        reason: Some("  integration  ".to_owned()),
        idempotency_key: idempotency_key.to_owned(),
        request_id: Some("req-ecosystem-1".to_owned()),
    }
}

#[tokio::test]
#[ignore = "requires an explicit CROWDRELAY_ECOSYSTEM_TEST_DATABASE_URL PostgreSQL database"]
async fn feature_flag_updates_are_idempotent_audited_and_replay_safe()
-> Result<(), Box<dyn std::error::Error>> {
    let database_url = std::env::var(TEST_DATABASE_URL_KEY)
        .map_err(|e| format!("set {TEST_DATABASE_URL_KEY}: {e}"))?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    database::MIGRATOR.run(&pool).await?;

    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Ecosystem E2E')")
        .bind(workspace_id.into_uuid())
        .bind(format!("ecosystem-{suffix}"))
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO ecosystem_feature_flags (workspace_id, key, enabled, reason) \
         VALUES ($1, 'ticket_sales_enabled', true, 'seed')",
    )
    .bind(workspace_id.into_uuid())
    .execute(&pool)
    .await?;

    let repository = PostgresEcosystemRepository::new(pool.clone());
    let key = format!("idem-{suffix}");

    // First write applies, bumps the version and trims the stored reason.
    let first = repository
        .update_feature_flag(&command(workspace_id, &key, false))
        .await?;
    assert!(!first.replayed);
    assert!(!first.flag.enabled);
    assert_eq!(first.flag.reason.as_deref(), Some("integration"));
    // The seeded row is version 1, so an applied update makes it 2.
    assert_eq!(first.flag.version, 2);

    // Same key, same payload: the stored outcome is returned without writing
    // a second time, so neither the version nor the audit trail moves.
    let replay = repository
        .update_feature_flag(&command(workspace_id, &key, false))
        .await?;
    assert!(replay.replayed);
    assert_eq!(replay.flag.version, 2);

    let actions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM operator_actions WHERE workspace_id = $1 AND idempotency_key = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind(&key)
    .fetch_one(&pool)
    .await?;
    assert_eq!(actions, 1, "a replay must not append a second audit row");

    // Same key, different payload: a reused key is a conflict, never a silent
    // overwrite of the earlier decision.
    let conflict = repository
        .update_feature_flag(&command(workspace_id, &key, true))
        .await
        .expect_err("reusing an idempotency key with a new payload must conflict");
    assert_eq!(conflict, EcosystemRepositoryError::Conflict);

    let enabled: bool = sqlx::query_scalar(
        "SELECT enabled FROM ecosystem_feature_flags WHERE workspace_id = $1 AND key = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind("ticket_sales_enabled")
    .fetch_one(&pool)
    .await?;
    assert!(!enabled, "the conflicting replay must not have flipped it");

    // A fresh key applies normally on top of the committed state.
    let second = repository
        .update_feature_flag(&command(workspace_id, &format!("{key}-b"), true))
        .await?;
    assert!(!second.replayed);
    assert!(second.flag.enabled);
    assert_eq!(second.flag.version, 3);

    // The declared-flag set is the caller's: update_flag rejects unknown keys
    // before reaching here, and this upsert must stay able to materialize a
    // declared flag on its first flip, because ensure_default_flags does not
    // run on the update path. So the repository creates the row it is asked
    // for, and the version starts at 1 rather than continuing someone else's.
    let created = repository
        .update_feature_flag(&UpdateFeatureFlagCommand {
            key: "mailer_enabled".to_owned(),
            idempotency_key: format!("{key}-c"),
            ..command(workspace_id, &key, true)
        })
        .await?;
    assert!(created.flag.enabled);
    assert_eq!(created.flag.version, 1);

    // --- checklist: same replay rules, different aggregate -------------------
    sqlx::query(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at) \
         VALUES ($1, $2, $3, 'Checklist show', now() + interval '30 days', 'published', now())",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(format!("show-{suffix}"))
    .execute(&pool)
    .await?;

    let checklist = |idem: &str, status: &str| UpdateShowChecklistCommand {
        workspace_id,
        event_slug: format!("show-{suffix}"),
        item_key: "load-in".to_owned(),
        status: status.to_owned(),
        note: Some("  backline at 16:00  ".to_owned()),
        idempotency_key: idem.to_owned(),
        request_id: Some("req-checklist-1".to_owned()),
    };

    let applied = repository
        .update_show_checklist(&checklist(&format!("{key}-cl"), "done"))
        .await?;
    assert!(!applied.replayed);
    let item = applied
        .items
        .iter()
        .find(|item| item.item_key == "load-in")
        .expect("the written item should come back");
    assert_eq!(item.status, "done");
    assert_eq!(item.note.as_deref(), Some("backline at 16:00"));

    let replayed = repository
        .update_show_checklist(&checklist(&format!("{key}-cl"), "done"))
        .await?;
    assert!(replayed.replayed);

    let conflict = repository
        .update_show_checklist(&checklist(&format!("{key}-cl"), "blocked"))
        .await
        .expect_err("reusing a checklist key with a new status must conflict");
    assert_eq!(conflict, EcosystemRepositoryError::Conflict);

    // An unresolvable event is reported, not silently created.
    let missing = repository
        .update_show_checklist(&UpdateShowChecklistCommand {
            event_slug: "no-such-show".to_owned(),
            idempotency_key: format!("{key}-cl-missing"),
            ..checklist(&format!("{key}-cl"), "done")
        })
        .await
        .expect_err("an unknown event slug must not create a checklist");
    assert_eq!(missing, EcosystemRepositoryError::NotFound);

    // --- reconciliation: a pass is never run twice for one key ---------------
    let reconcile = |idem: &str, trigger: &str| RunReconciliationCommand {
        workspace_id,
        trigger: trigger.to_owned(),
        idempotency_key: idem.to_owned(),
        request_id: Some("req-reconcile-1".to_owned()),
    };

    let first_pass = repository
        .run_reconciliation(&reconcile(&format!("{key}-rec"), "manual"))
        .await?;
    assert!(!first_pass.replayed);
    assert_eq!(first_pass.run.status, "completed");
    assert_eq!(first_pass.run.trigger, "manual");
    assert!(first_pass.run.finished_at.is_some());
    assert_eq!(
        first_pass.run.finding_count as usize,
        first_pass.findings.len(),
        "the stored count must match the findings the run actually raised"
    );

    // A replay returns the original run rather than starting a second pass, so
    // findings cannot be double-counted and their outbox events not re-emitted.
    let replayed_pass = repository
        .run_reconciliation(&reconcile(&format!("{key}-rec"), "manual"))
        .await?;
    assert!(replayed_pass.replayed);
    assert_eq!(replayed_pass.run.id, first_pass.run.id);

    let runs: i64 =
        sqlx::query_scalar("SELECT count(*) FROM reconciliation_runs WHERE workspace_id = $1")
            .bind(workspace_id.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        runs, 1,
        "a replayed reconciliation must not start a new run"
    );

    // The same key with a different trigger is a different request.
    let conflict = repository
        .run_reconciliation(&reconcile(&format!("{key}-rec"), "scheduled"))
        .await
        .expect_err("reusing a reconciliation key with a new trigger must conflict");
    assert_eq!(conflict, EcosystemRepositoryError::Conflict);

    // A fresh key starts a genuinely new pass.
    let second_pass = repository
        .run_reconciliation(&reconcile(&format!("{key}-rec2"), "deploy"))
        .await?;
    assert!(!second_pass.replayed);
    assert_ne!(second_pass.run.id, first_pass.run.id);

    Ok(())
}
