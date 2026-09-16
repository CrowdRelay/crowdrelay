//! Suggestion-queue lifecycle, against a real schema — the taste signal
//! and the unreported-commitment sweep. `refresh_suggestions` owns the
//! queue's truth: these tests prove a decline suppresses per tenant and
//! ages out, and that an approved beat whose day passed resolves to an
//! outcome instead of holding a slot forever.

use crowdrelay_domain::{WorkspaceId, content_engine::SuggestionOutcomeKind};
use crowdrelay_infra::content_engine::{
    NewOutcome, NewSuggestion, PostgresContentEngineRepository,
};
use serde_json::json;
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

async fn repository()
-> Result<(PostgresContentEngineRepository, PgPool), Box<dyn std::error::Error>> {
    let database_url = std::env::var("CROWDRELAY_TEST_DATABASE_URL")
        .map_err(|e| format!("CROWDRELAY_TEST_DATABASE_URL must be configured: {e}"))?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
    Ok((PostgresContentEngineRepository::new(pool.clone()), pool))
}

/// Every workspace table now `REFERENCES workspaces(id)` — a test workspace
/// has to exist before rows under it can land.
async fn seed_workspace(
    pool: &PgPool,
    workspace_id: WorkspaceId,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!(
            "suggestion-queue-{}",
            workspace_id.into_uuid().simple()
        ))
        .bind("Suggestion Queue Test")
        .execute(pool)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn a_declined_format_stays_out_until_the_verdict_ages_out()
-> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;

    // An active arc pins the feasible set to exactly one format — the
    // spine names `playthrough`, no production event exists to free an
    // orphan, so every raise below is deterministic.
    let seed_band = |pool: &PgPool, workspace_id: WorkspaceId, suffix: &str| {
        let pool = pool.clone();
        let suffix = suffix.to_owned();
        async move {
            let member_id: Uuid = sqlx::query_scalar(
                "INSERT INTO workspace_members (workspace_id, normalized_email, role, status)
                 VALUES ($1, $2, 'staff', 'active') RETURNING id",
            )
            .bind(workspace_id.into_uuid())
            .bind(format!("crew-{suffix}@example.test"))
            .fetch_one(&pool)
            .await?;
            sqlx::query(
                "INSERT INTO viryaos_team_profiles
                     (workspace_id, member_id, member_key, active, skills)
                 VALUES ($1, $2, 'crew', true,
                         ARRAY['general','operations','booking','approval','technical',
                               'visual','video','photography','social','english_copy',
                               'polish_copy','people']::text[])",
            )
            .bind(workspace_id.into_uuid())
            .bind(member_id)
            .execute(&pool)
            .await?;
            sqlx::query(
                "INSERT INTO viryaos_release_plans (workspace_id, source_key, title, release_at, active)
                 VALUES ($1, $2, 'Taste Release', now() + interval '14 days', true)",
            )
            .bind(workspace_id.into_uuid())
            .bind(format!("taste-rel-{suffix}"))
            .execute(&pool)
            .await?;
            sqlx::query(
                "INSERT INTO discovery_places (workspace_id, place_kind, platform, name, url, status)
                 VALUES ($1, 'subreddit', 'reddit', 'r/Metal', $2, 'active')",
            )
            .bind(workspace_id.into_uuid())
            .bind(format!("https://reddit.com/r/metal-{suffix}"))
            .execute(&pool)
            .await?;
            let fan_id: Uuid = sqlx::query_scalar(
                "INSERT INTO fans (workspace_id, normalized_email, status)
                 VALUES ($1, $2, 'active') RETURNING id",
            )
            .bind(workspace_id.into_uuid())
            .bind(format!("fan-{suffix}@example.test"))
            .fetch_one(&pool)
            .await?;
            sqlx::query(
                "INSERT INTO fan_consents (workspace_id, fan_id, purpose, granted, policy_version, source)
                 VALUES ($1, $2, 'marketing', true, 'v1', 'test')",
            )
            .bind(workspace_id.into_uuid())
            .bind(fan_id)
            .execute(&pool)
            .await?;
            sqlx::query(
                "INSERT INTO viryaos_arcs (id, workspace_id, title, spine, status,
                                          horizon_start, horizon_end)
                 VALUES ($1, $2, 'Taste arc', $3, 'active',
                         current_date, current_date + interval '60 days')",
            )
            .bind(Uuid::now_v7())
            .bind(workspace_id.into_uuid())
            .bind(json!([
                {"at": "2026-10-10", "beat": "playthrough", "format_key": "playthrough"}
            ]))
            .execute(&pool)
            .await?;
            Ok::<(), Box<dyn std::error::Error>>(())
        }
    };

    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;
    let suffix = workspace_id.into_uuid().simple().to_string();
    seed_band(&pool, workspace_id, &suffix).await?;

    let today = time::OffsetDateTime::now_utc().date();
    let raised = repo.refresh_suggestions(workspace_id, today).await?;
    assert_eq!(raised.len(), 1);
    assert_eq!(raised[0].format_key.as_deref(), Some("playthrough"));

    // The band says "not for us" — recorded as a first-class outcome.
    repo.resolve_suggestion(
        workspace_id,
        &NewOutcome {
            suggestion_id: raised[0].id,
            outcome: SuggestionOutcomeKind::Declined,
            decided_by: Some("operator".to_owned()),
            reason: Some("not for us".to_owned()),
            results: json!({}),
        },
    )
    .await?;

    // Inside the cooldown the verdict holds: nothing is re-asked — the
    // honest queue is empty rather than a repeat of a refused concept.
    let after = repo.refresh_suggestions(workspace_id, today).await?;
    assert!(
        after.is_empty(),
        "a declined format re-appeared inside the cooldown: {:?}",
        after.iter().map(|s| &s.format_key).collect::<Vec<_>>()
    );

    // The verdict is per tenant — an identical second band still hears
    // the same suggestion its peer just refused.
    let other = WorkspaceId::new();
    seed_workspace(&pool, other).await?;
    let osuffix = other.into_uuid().simple().to_string();
    seed_band(&pool, other, &osuffix).await?;
    let theirs = repo.refresh_suggestions(other, today).await?;
    assert!(
        theirs
            .iter()
            .any(|s| s.format_key.as_deref() == Some("playthrough")),
        "one tenant's 'not for us' must not censor another tenant's queue"
    );

    // Past the cooldown the verdict ages out — evidence can argue the
    // format back and the same deterministic raise returns.
    sqlx::query(
        "UPDATE viryaos_suggestion_outcomes
         SET resolved_at = now() - interval '43 days'
         WHERE workspace_id = $1",
    )
    .bind(workspace_id.into_uuid())
    .execute(&pool)
    .await?;
    let revived = repo.refresh_suggestions(workspace_id, today).await?;
    assert!(
        revived
            .iter()
            .any(|s| s.format_key.as_deref() == Some("playthrough")),
        "a verdict whose window lapsed must not suppress forever"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn an_unreported_commitment_expires_when_its_day_passes()
-> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;

    let today = time::OffsetDateTime::now_utc().date();
    // The band committed; the beat's day was yesterday; nobody reported.
    let suggestion = repo
        .create_suggestion(
            workspace_id,
            &NewSuggestion {
                arc_id: None,
                format_key: Some("playthrough".to_owned()),
                concept: "film the playthrough during the shoot".to_owned(),
                reason: "peer playthroughs outperform 4:1".to_owned(),
                evidence: json!({}),
                suggested_after: Some(today - time::Duration::days(7)),
                suggested_before: Some(today - time::Duration::days(1)),
                effort: None,
                proposed_assignee_member_id: None,
                distribution_promise: json!({"consented_fans": 340}),
                expires_at: None,
            },
        )
        .await?;
    sqlx::query("UPDATE viryaos_content_suggestions SET status = 'approved' WHERE id = $1")
        .bind(suggestion.id.into_uuid())
        .execute(&pool)
        .await?;

    repo.refresh_suggestions(workspace_id, today).await?;

    let (status, outcomes): (String, i64) = sqlx::query_as(
        "SELECT s.status,
                (SELECT count(*) FROM viryaos_suggestion_outcomes o
                  WHERE o.workspace_id = s.workspace_id AND o.suggestion_id = s.id
                    AND o.outcome = 'expired')
         FROM viryaos_content_suggestions s WHERE s.id = $1",
    )
    .bind(suggestion.id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        (status.as_str(), outcomes),
        ("expired", 1),
        "an unreported commitment must resolve to an outcome, not hold a queue slot forever"
    );
    let (decided_by, reason): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT decided_by, reason FROM viryaos_suggestion_outcomes
         WHERE workspace_id = $1 AND suggestion_id = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind(suggestion.id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(decided_by.as_deref(), Some("system"));
    assert!(
        reason.unwrap_or_default().contains("unmeasured"),
        "the label says what is unknown rather than guess a done"
    );

    // A timeless commitment (`suggested_before` NULL) stays open — there is
    // no day whose passing makes it dead.
    let timeless = repo
        .create_suggestion(
            workspace_id,
            &NewSuggestion {
                arc_id: None,
                format_key: None,
                concept: "record an acoustic session whenever".to_owned(),
                reason: "unbounded".to_owned(),
                evidence: json!({}),
                suggested_after: None,
                suggested_before: None,
                effort: None,
                proposed_assignee_member_id: None,
                distribution_promise: json!({"consented_fans": 340}),
                expires_at: None,
            },
        )
        .await?;
    sqlx::query("UPDATE viryaos_content_suggestions SET status = 'approved' WHERE id = $1")
        .bind(timeless.id.into_uuid())
        .execute(&pool)
        .await?;
    repo.refresh_suggestions(workspace_id, today).await?;
    let still_open: String =
        sqlx::query_scalar("SELECT status FROM viryaos_content_suggestions WHERE id = $1")
            .bind(timeless.id.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(still_open, "approved");
    Ok(())
}
