//! Live-Postgres coverage of the arc engine: the proposer writes one
//! season from a real anchor, the lifecycle moves it without an operator,
//! and an active arc refuses orphan suggestions end to end.

use crate::common;
use crowdrelay_domain::{ArcId, WorkspaceId, content_engine::ArcStatus};
use crowdrelay_infra::content_engine::PostgresContentEngineRepository;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

/// The season rows the lifecycle tests need — status transitions here are
/// fixture steps, not the subject, so they write the row the operator path
/// would leave. `id` and `spine` come back for the assertions that follow.
async fn seed_arc(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    status: &str,
    spine: serde_json::Value,
) -> Result<(ArcId, serde_json::Value), Box<dyn std::error::Error>> {
    let id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO arcs (id, workspace_id, title, summary, spine, evidence,
                           horizon_start, horizon_end)
         VALUES ($1, $2, 'chosen season', 'fixture', $3, '{}'::jsonb,
                 current_date, current_date + interval '40 days')
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(&spine)
    .fetch_one(pool)
    .await?;
    if status != "proposed" {
        sqlx::query("UPDATE arcs SET status = $3 WHERE id = $1 AND workspace_id = $2")
            .bind(id)
            .bind(workspace_id.into_uuid())
            .bind(status)
            .execute(pool)
            .await?;
    }
    Ok((ArcId::from_uuid(id), spine))
}

/// Status-filtered arc ids — what the removed `list_arcs` read proved:
/// lifecycle wrote the row the filter expects.
async fn arcs_with_status(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    status: &str,
) -> Result<Vec<(Uuid, serde_json::Value)>, Box<dyn std::error::Error>> {
    Ok(sqlx::query_as::<_, (Uuid, serde_json::Value)>(
        "SELECT id, spine FROM arcs WHERE workspace_id = $1 AND status = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind(status)
    .fetch_all(pool)
    .await?)
}

async fn repository()
-> Result<(PostgresContentEngineRepository, PgPool), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
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
            "content-arcs-{}",
            workspace_id.into_uuid().simple()
        ))
        .bind("Content Arcs Test")
        .execute(pool)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn refresh_arcs_proposes_once_then_the_lifecycle_moves_it()
-> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;

    // Capability: one active member carrying the skills the catalogue asks
    // for. Without this the proposer sees zero feasible formats and the arc
    // would be a playlist refusal, not a plan.
    let member_id: Uuid = sqlx::query_scalar(
        "INSERT INTO workspace_members (workspace_id, normalized_email, role, status)
         VALUES ($1, $2, 'staff', 'active') RETURNING id",
    )
    .bind(workspace_id.into_uuid())
    .bind(format!(
        "crew-{}@example.test",
        workspace_id.into_uuid().simple()
    ))
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO team_profiles
             (workspace_id, member_id, member_key, active, skills)
         VALUES ($1, $2, 'crew', true, ARRAY['video','photography','social','english_copy']::text[])",
    )
    .bind(workspace_id.into_uuid())
    .bind(member_id)
    .execute(&pool)
    .await?;

    // The anchor: a release four weeks out — inside the proposal window.
    let release_id: Uuid = sqlx::query_scalar(
        "INSERT INTO release_plans (workspace_id, source_key, title, release_at, active)
         VALUES ($1, 'arc-test', 'Arc Single', now() + interval '30 days', true)
         RETURNING id",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&pool)
    .await?;

    let today = time::OffsetDateTime::now_utc().date();
    let proposed = repo.refresh_arcs(workspace_id, today).await?;
    assert_eq!(proposed.len(), 1, "one anchor in window means one arc");
    let arc = &proposed[0];
    assert_eq!(arc.status, ArcStatus::Proposed);
    assert_eq!(
        arc.evidence["anchor"]["id"].as_str(),
        Some(release_id.to_string().as_str()),
        "the arc's evidence names the row it rests on"
    );
    let beats = arc.spine.as_array().expect("spine is an array");
    assert!(
        beats.len() >= 2,
        "an arc is a story — one beat is a playlist, refused"
    );
    assert!(
        beats
            .iter()
            .all(|beat| beat["format_key"].as_str().is_some()),
        "every beat names the catalogue format it fills"
    );

    // The dedup rule: while anything is open — proposed, approved or
    // active — no second season is invented.
    assert!(
        repo.refresh_arcs(workspace_id, today).await?.is_empty(),
        "an open arc is the season; a second ask is noise"
    );

    // Approval is the band's one decision; activation follows the horizon.
    sqlx::query(
        "UPDATE arcs SET status = 'approved', approved_at = now(), approved_by = 'operator', updated_at = now()
         WHERE id = $1 AND workspace_id = $2 AND status = 'proposed'",
    )
    .bind(arc.id.into_uuid())
    .bind(workspace_id.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("UPDATE arcs SET horizon_start = $2 WHERE id = $1")
        .bind(arc.id.into_uuid())
        .bind(today)
        .execute(&pool)
        .await?;
    assert!(
        repo.refresh_arcs(workspace_id, today).await?.is_empty(),
        "an approved arc is still the open season"
    );
    let active = arcs_with_status(&pool, workspace_id, "active").await?;
    assert_eq!(
        active.len(),
        1,
        "an approved arc goes live when its horizon opens"
    );

    // Its spine is what the suggestion engine reads — `arc_format_keys`
    // scans this same JSON for approved and active rows.
    let spine_keys: Vec<&str> = active[0]
        .1
        .as_array()
        .expect("spine")
        .iter()
        .filter_map(|beat| beat["format_key"].as_str())
        .collect();
    assert!(
        spine_keys.len() >= 2,
        "the active arc's spine feeds the ranker: {spine_keys:?}"
    );

    // The horizon closes: the season completes, and the lifecycle moves it
    // without an operator touching anything.
    sqlx::query("UPDATE arcs SET horizon_start = $2, horizon_end = $3 WHERE id = $1")
        .bind(arc.id.into_uuid())
        .bind(today - time::Duration::days(10))
        .bind(today - time::Duration::days(1))
        .execute(&pool)
        .await?;
    repo.refresh_arcs(workspace_id, today).await?;
    let completed = arcs_with_status(&pool, workspace_id, "completed").await?;
    assert_eq!(completed.len(), 1, "a finished season marks itself done");

    // A completed season does not blacklist its anchor — but a retired one
    // does, for the cooldown. Retire the fresh proposal and confirm the
    // anchor does not re-ask inside the window.
    let fresh = arcs_with_status(&pool, workspace_id, "proposed").await?;
    assert_eq!(
        fresh.len(),
        1,
        "with the season done a new proposal may land"
    );
    sqlx::query("UPDATE arcs SET status = 'retired', updated_at = now() WHERE id = $1")
        .bind(fresh[0].0)
        .execute(&pool)
        .await?;
    assert!(
        repo.refresh_arcs(workspace_id, today).await?.is_empty(),
        "a 'no' inside the cooldown is still a no"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn an_active_arc_refuses_orphan_suggestions_end_to_end()
-> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;
    let suffix = workspace_id.into_uuid().simple().to_string();

    let member_id: Uuid = sqlx::query_scalar(
        "INSERT INTO workspace_members (workspace_id, normalized_email, role, status)
         VALUES ($1, $2, 'staff', 'active') RETURNING id",
    )
    .bind(workspace_id.into_uuid())
    .bind(format!("crew-{suffix}@example.test"))
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO team_profiles
             (workspace_id, member_id, member_key, active, skills)
         VALUES ($1, $2, 'crew', true, ARRAY['video','photography','social','english_copy']::text[])",
    )
    .bind(workspace_id.into_uuid())
    .bind(member_id)
    .execute(&pool)
    .await?;

    // Reach so the promise has real clauses — no communities, no queue.
    sqlx::query(
        "INSERT INTO discovery_places (workspace_id, place_kind, platform, name, url, status)
         VALUES ($1, 'subreddit', 'reddit', 'r/Metal', $2, 'active')",
    )
    .bind(workspace_id.into_uuid())
    .bind(format!("https://reddit.com/r/metal-{suffix}"))
    .execute(&pool)
    .await?;

    // Release material — the spine's formats require it, and the capability
    // gate answers from the calendar, not from the arc itself.
    sqlx::query(
        "INSERT INTO release_plans (workspace_id, source_key, title, release_at, active)
         VALUES ($1, 'arc-suppress', 'Arc Single', now() + interval '30 days', true)",
    )
    .bind(workspace_id.into_uuid())
    .execute(&pool)
    .await?;

    // The season the band already chose: two spine beats, status active.
    // With no production day scheduled, nothing is time-boxed — so the only
    // suggestions allowed through are the arc's own formats.
    let arc_id = seed_arc(
        &pool,
        workspace_id,
        "active",
        json!([
            {"at": "2026-10-10", "beat": "Playthrough", "format_key": "playthrough"},
            {"at": "2026-10-20", "beat": "Making of", "format_key": "making_of"}
        ]),
    )
    .await?
    .0;

    let today = time::OffsetDateTime::now_utc().date();
    let raised = repo.refresh_suggestions(workspace_id, today).await?;
    assert!(
        !raised.is_empty(),
        "spine beats still surface — the arc lifts its own"
    );
    for suggestion in &raised {
        assert_eq!(
            suggestion.arc_id,
            Some(arc_id),
            "with a season running, only its beats reach the queue: {}",
            suggestion.format_key.as_deref().unwrap_or("?")
        );
    }
    Ok(())
}

/// §4b-4's stale rule applied to the season spine: a format that burned
/// six offers without producing is ineligible as an arc beat. The
/// polarity matters — an inverted predicate keeps the stale and drops
/// the viable, so the proof runs both directions: all-stale proposes
/// nothing, and a mostly-stale workspace proposes only living formats.
async fn seed_capability_and_anchor(
    pool: &PgPool,
    workspace_id: WorkspaceId,
) -> Result<(), Box<dyn std::error::Error>> {
    let member_id: Uuid = sqlx::query_scalar(
        "INSERT INTO workspace_members (workspace_id, normalized_email, role, status)
         VALUES ($1, $2, 'staff', 'active') RETURNING id",
    )
    .bind(workspace_id.into_uuid())
    .bind(format!(
        "crew-{}@example.test",
        workspace_id.into_uuid().simple()
    ))
    .fetch_one(pool)
    .await?;
    sqlx::query(
        "INSERT INTO team_profiles
             (workspace_id, member_id, member_key, active, skills)
         VALUES ($1, $2, 'crew', true, ARRAY['video','photography','social','english_copy']::text[])",
    )
    .bind(workspace_id.into_uuid())
    .bind(member_id)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO release_plans (workspace_id, source_key, title, release_at, active)
         VALUES ($1, 'arc-test', 'Arc Single', now() + interval '30 days', true)",
    )
    .bind(workspace_id.into_uuid())
    .execute(pool)
    .await?;
    Ok(())
}

/// Six offers, zero productions — the catalogue entry is retired for
/// this workspace. `keep` names the formats left living.
async fn retire_formats(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    keep: &[&str],
) -> Result<(), Box<dyn std::error::Error>> {
    let keys: Vec<String> = sqlx::query_scalar("SELECT key FROM content_format_entries")
        .fetch_all(pool)
        .await?;
    for key in keys.iter().filter(|k| !keep.contains(&k.as_str())) {
        for _ in 0..6 {
            let suggestion_id: Uuid = sqlx::query_scalar(
                "INSERT INTO content_suggestions
                     (id, workspace_id, format_key, concept, status)
                 VALUES ($1, $2, $3, 'stale beat', 'expired') RETURNING id",
            )
            .bind(Uuid::now_v7())
            .bind(workspace_id.into_uuid())
            .bind(key)
            .fetch_one(pool)
            .await?;
            sqlx::query(
                "INSERT INTO suggestion_outcomes
                     (workspace_id, suggestion_id, outcome, resolved_at)
                 VALUES ($1, $2, 'expired', now() - interval '50 days')",
            )
            .bind(workspace_id.into_uuid())
            .bind(suggestion_id)
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn a_retired_format_cannot_anchor_an_arc_beat() -> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let today = time::OffsetDateTime::now_utc().date();

    // All-stale: every catalogue format burned its offers. Feasibility
    // must be empty — under the inverted predicate this workspace
    // proposed happily, so silence is the polarity proof.
    let all_stale = WorkspaceId::new();
    seed_workspace(&pool, all_stale).await?;
    seed_capability_and_anchor(&pool, all_stale).await?;
    retire_formats(&pool, all_stale, &[]).await?;
    assert!(
        repo.refresh_arcs(all_stale, today).await?.is_empty(),
        "every format retired means no season — the spine cannot rest on a dead concept"
    );

    // Mostly-stale: three living formats among the retired. The arc
    // still proposes, and every beat names a living format.
    let mixed = WorkspaceId::new();
    seed_workspace(&pool, mixed).await?;
    seed_capability_and_anchor(&pool, mixed).await?;
    let living = ["rehearsal_clip", "track_by_track", "lyric_video"];
    retire_formats(&pool, mixed, &living).await?;
    let proposed = repo.refresh_arcs(mixed, today).await?;
    assert_eq!(proposed.len(), 1, "living formats still carry a season");
    let beats = proposed[0].spine.as_array().expect("spine is an array");
    assert!(
        beats.len() >= 2,
        "an arc is a story — one beat is a playlist, refused"
    );
    for beat in beats {
        let key = beat["format_key"].as_str().unwrap_or_default();
        assert!(
            living.contains(&key),
            "a retired format reached the spine: {key}"
        );
    }
    Ok(())
}
