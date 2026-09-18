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

    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;
    let suffix = workspace_id.into_uuid().simple().to_string();
    // An active arc pins the feasible set to exactly one format — the
    // spine names `playthrough`, no production event exists to free an
    // orphan, so every raise below is deterministic.
    seed_band(&pool, workspace_id, &suffix, one_beat_spine("playthrough")).await?;

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
    seed_band(&pool, other, &osuffix, one_beat_spine("playthrough")).await?;
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

/// The band the deterministic-arc fixtures need: one skilled member, a
/// release in two weeks, a live community room, a consented fan — then an
/// active arc whose `spine` names the feasible set.
async fn seed_band(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    suffix: &str,
    spine: serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
    let member_id: Uuid = sqlx::query_scalar(
        "INSERT INTO workspace_members (workspace_id, normalized_email, role, status)
                 VALUES ($1, $2, 'staff', 'active') RETURNING id",
    )
    .bind(workspace_id.into_uuid())
    .bind(format!("crew-{suffix}@example.test"))
    .fetch_one(pool)
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
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO viryaos_release_plans (workspace_id, source_key, title, release_at, active)
                 VALUES ($1, $2, 'Taste Release', now() + interval '14 days', true)",
    )
    .bind(workspace_id.into_uuid())
    .bind(format!("taste-rel-{suffix}"))
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO discovery_places (workspace_id, place_kind, platform, name, url, status)
                 VALUES ($1, 'subreddit', 'reddit', 'r/Metal', $2, 'active')",
    )
    .bind(workspace_id.into_uuid())
    .bind(format!("https://reddit.com/r/metal-{suffix}"))
    .execute(pool)
    .await?;
    let fan_id: Uuid = sqlx::query_scalar(
        "INSERT INTO fans (workspace_id, normalized_email, status)
                 VALUES ($1, $2, 'active') RETURNING id",
    )
    .bind(workspace_id.into_uuid())
    .bind(format!("fan-{suffix}@example.test"))
    .fetch_one(pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_consents (workspace_id, fan_id, purpose, granted, policy_version, source)
                 VALUES ($1, $2, 'marketing', true, 'v1', 'test')",
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO viryaos_arcs (id, workspace_id, title, spine, status,
                                          horizon_start, horizon_end)
                 VALUES ($1, $2, 'Taste arc', $3, 'active',
                         current_date, current_date + interval '60 days')",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(&spine)
    .execute(pool)
    .await?;
    Ok(())
}

/// A one-beat spine pinning the feasible set to a single format.
fn one_beat_spine(format_key: &str) -> serde_json::Value {
    json!([{"at": "2026-10-10", "beat": format_key, "format_key": format_key}])
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

/// §4b-4 — six offers without a single production retires the concept:
/// the ranker drops it before scoring, so the queue stops offering it
/// while its spine-mates still raise. `expired` outcomes seed the
/// history rather than `declined`, so the taste cooldown cannot explain
/// the silence — only the stale rule can. And because the feasible set
/// outnumbers the queue, every raised row names the tail it cut.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn a_concept_that_never_produced_retires_and_the_tail_is_named()
-> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;
    let suffix = workspace_id.into_uuid().simple().to_string();
    let today = time::OffsetDateTime::now_utc().date();

    // Five spine beats — more feasible formats than the queue has room
    // for, so the cut leaves a tail the raised rows must name.
    let spine = |offset: i64| today + time::Duration::days(offset);
    seed_band(
        &pool,
        workspace_id,
        &suffix,
        json!([
            {"at": spine(10).to_string(), "beat": "playthrough", "format_key": "playthrough"},
            {"at": spine(11).to_string(), "beat": "rehearsal", "format_key": "rehearsal_clip"},
            {"at": spine(12).to_string(), "beat": "track", "format_key": "track_by_track"},
            {"at": spine(13).to_string(), "beat": "making", "format_key": "making_of"},
            {"at": spine(14).to_string(), "beat": "lyric", "format_key": "lyric_video"}
        ]),
    )
    .await?;

    // Playthrough has been offered six times; every offer lapsed without
    // the band making anything. Six `expired` outcomes, zero produced —
    // the concept has had its chances.
    for _ in 0..6 {
        let suggestion_id: Uuid = sqlx::query_scalar(
            "INSERT INTO viryaos_content_suggestions
                 (id, workspace_id, format_key, concept, status)
             VALUES ($1, $2, 'playthrough', 'stale playthrough beat', 'expired')
             RETURNING id",
        )
        .bind(Uuid::now_v7())
        .bind(workspace_id.into_uuid())
        .fetch_one(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO viryaos_suggestion_outcomes
                 (workspace_id, suggestion_id, outcome, resolved_at)
             VALUES ($1, $2, 'expired', now() - interval '50 days')",
        )
        .bind(workspace_id.into_uuid())
        .bind(suggestion_id)
        .execute(&pool)
        .await?;
    }

    let raised = repo.refresh_suggestions(workspace_id, today).await?;
    let raised_keys: Vec<Option<&str>> = raised.iter().map(|s| s.format_key.as_deref()).collect();
    assert!(
        !raised_keys.contains(&Some("playthrough")),
        "six offers, zero productions — the concept is retired, not re-raised: {raised_keys:?}"
    );
    assert!(
        !raised.is_empty(),
        "the surviving spine beats still raise: {raised_keys:?}"
    );

    // §4b-4's second half — the operator can see the cut: every raised
    // row's reason names the tail count and the concepts it dropped, and
    // the evidence carries the same list as data.
    let tail = raised[0].evidence.get("tail").cloned().unwrap_or_default();
    let tail_count = tail.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
    let tail_concepts: Vec<String> = tail
        .get("concepts")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(
        raised.len() + tail_count as usize,
        4,
        "raised + named tail must account for every feasible spine beat          (5 minus retired playthrough): raised={raised_keys:?} tail={tail_concepts:?}"
    );
    assert!(
        raised
            .iter()
            .all(|s| s.reason.contains("other feasible format")),
        "the tail is named in the row the operator reads, not only in a log"
    );
    assert!(
        !tail_concepts.iter().any(|c| c.contains("laythrough")),
        "a retired concept is not 'feasible' — the tail must not name it: {tail_concepts:?}"
    );

    // The paired band: same six offers, but one produced — `done` once is
    // evidence the format can land, so the stale rule does not retire it.
    let other = WorkspaceId::new();
    seed_workspace(&pool, other).await?;
    let osuffix = other.into_uuid().simple().to_string();
    seed_band(
        &pool,
        other,
        &osuffix,
        json!([
            {"at": spine(10).to_string(), "beat": "playthrough", "format_key": "playthrough"}
        ]),
    )
    .await?;
    for index in 0..6 {
        let produced = index == 0;
        let suggestion_id: Uuid = sqlx::query_scalar(
            "INSERT INTO viryaos_content_suggestions
                 (id, workspace_id, format_key, concept, status)
             VALUES ($1, $2, 'playthrough', 'playthrough beat', $3)
             RETURNING id",
        )
        .bind(Uuid::now_v7())
        .bind(other.into_uuid())
        .bind(if produced { "done" } else { "expired" })
        .fetch_one(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO viryaos_suggestion_outcomes
                 (workspace_id, suggestion_id, outcome, resolved_at)
             VALUES ($1, $2, $3, now() - interval '50 days')",
        )
        .bind(other.into_uuid())
        .bind(suggestion_id)
        .bind(if produced { "done" } else { "expired" })
        .execute(&pool)
        .await?;
    }
    let theirs = repo.refresh_suggestions(other, today).await?;
    assert!(
        theirs
            .iter()
            .any(|s| s.format_key.as_deref() == Some("playthrough")),
        "one production in six offers keeps the concept in the queue"
    );
    Ok(())
}

/// 5.6 — shared learning, respecting per-band taste. A same-style sibling's
/// productions pool into the target act's prior and the raised row names the
/// evidence; a different-style labelmate and an org-less lookalike teach
/// nothing; an act that never declared a style pools nothing.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn a_same_style_siblings_productions_lift_the_format()
-> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;

    let organization_id = Uuid::now_v7();
    sqlx::query("INSERT INTO organizations (id, slug, name) VALUES ($1, $2, $2)")
        .bind(organization_id)
        .bind("roster-label")
        .execute(&pool)
        .await?;

    // The act being advised: full band fixture, two-format spine so both
    // beats are feasible, and a declared style written messily on purpose —
    // the sibling's tidier spelling must still match it.
    let target = WorkspaceId::new();
    seed_workspace(&pool, target).await?;
    sqlx::query("UPDATE workspaces SET organization_id = $2 WHERE id = $1")
        .bind(target.into_uuid())
        .bind(organization_id)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO tenant_settings (workspace_id, key, value) VALUES ($1, 'act_style', 'Stoner  DOOM')",
    )
    .bind(target.into_uuid())
    .execute(&pool)
    .await?;
    let tsuffix = target.into_uuid().simple().to_string();
    seed_band(
        &pool,
        target,
        &tsuffix,
        json!([
            {"at": "2026-10-10", "beat": "playthrough", "format_key": "playthrough"},
            {"at": "2026-10-17", "beat": "rehearsal_clip", "format_key": "rehearsal_clip"}
        ]),
    )
    .await?;

    // Two productions on the sibling's ledger — the floor the lift needs.
    async fn produced_twice(
        pool: &PgPool,
        workspace_id: Uuid,
        format_key: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        for _ in 0..2 {
            let suggestion_id: Uuid = sqlx::query_scalar(
                "INSERT INTO viryaos_content_suggestions
                     (id, workspace_id, format_key, concept, status)
                 VALUES ($1, $2, $3, 'sibling beat', 'done')
                 RETURNING id",
            )
            .bind(Uuid::now_v7())
            .bind(workspace_id)
            .bind(format_key)
            .fetch_one(pool)
            .await?;
            sqlx::query(
                "INSERT INTO viryaos_suggestion_outcomes
                     (workspace_id, suggestion_id, outcome, resolved_at)
                 VALUES ($1, $2, 'done', now() - interval '10 days')",
            )
            .bind(workspace_id)
            .bind(suggestion_id)
            .execute(pool)
            .await?;
        }
        Ok(())
    }

    async fn member(
        pool: &PgPool,
        organization_id: Option<Uuid>,
        style: &str,
    ) -> Result<Uuid, Box<dyn std::error::Error>> {
        let id = WorkspaceId::new();
        sqlx::query(
            "INSERT INTO workspaces (id, slug, name, organization_id) VALUES ($1, $2, $2, $3)",
        )
        .bind(id.into_uuid())
        .bind(format!("member-{}", id.into_uuid().simple()))
        .bind(organization_id)
        .execute(pool)
        .await?;
        sqlx::query(
            "INSERT INTO tenant_settings (workspace_id, key, value) VALUES ($1, 'act_style', $2)",
        )
        .bind(id.into_uuid())
        .bind(style)
        .execute(pool)
        .await?;
        Ok(id.into_uuid())
    }

    // Same style, same org — teaches. Different style, same org — does not.
    // Same style, no org — cannot even be seen.
    let sibling = member(&pool, Some(organization_id), "stoner doom").await?;
    produced_twice(&pool, sibling, "playthrough").await?;
    let other_style = member(&pool, Some(organization_id), "black metal").await?;
    produced_twice(&pool, other_style, "playthrough").await?;
    let outsider = member(&pool, None, "stoner doom").await?;
    produced_twice(&pool, outsider, "playthrough").await?;

    let today = time::OffsetDateTime::now_utc().date();
    let raised = repo.refresh_suggestions(target, today).await?;
    let proven = raised
        .iter()
        .find(|s| s.format_key.as_deref() == Some("playthrough"))
        .ok_or("the spine's proven beat is raised")?;
    assert_eq!(
        proven.evidence.get("sibling_productions"),
        Some(&json!(2)),
        "only the same-style sibling's two productions count — not the \
         black-metal labelmate's, not the org-less lookalike's"
    );
    assert!(
        proven.reason.contains("same-style acts"),
        "the raised row says where the prior came from: {}",
        proven.reason
    );

    // A workspace with no declared style pools nothing — the prior is
    // honest absence, not a borrowed one.
    let unstyled = WorkspaceId::new();
    seed_workspace(&pool, unstyled).await?;
    sqlx::query("UPDATE workspaces SET organization_id = $2 WHERE id = $1")
        .bind(unstyled.into_uuid())
        .bind(organization_id)
        .execute(&pool)
        .await?;
    let usuffix = unstyled.into_uuid().simple().to_string();
    seed_band(
        &pool,
        unstyled,
        &usuffix,
        json!([
            {"at": "2026-10-10", "beat": "playthrough", "format_key": "playthrough"},
            {"at": "2026-10-17", "beat": "rehearsal_clip", "format_key": "rehearsal_clip"}
        ]),
    )
    .await?;
    let theirs = repo.refresh_suggestions(unstyled, today).await?;
    assert!(
        theirs
            .iter()
            .all(|s| s.evidence.get("sibling_productions") == Some(&json!(0))),
        "an act that never declared a style inherits nothing"
    );
    Ok(())
}
