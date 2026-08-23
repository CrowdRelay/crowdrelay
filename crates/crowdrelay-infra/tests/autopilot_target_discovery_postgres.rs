use std::time::Duration;

use crowdrelay_application::{
    IdempotencyKey, RepositoryError,
    autopilot::{
        AutopilotTargetDiscoveryRepository, IngestOutreachCandidate, UpsertSubmissionChannel,
    },
};
use crowdrelay_domain::{
    WorkspaceId,
    outreach::OutreachTargetKind,
    target_discovery::{CandidateSource, ChannelCost, RouteKind},
};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

/// Discovery is only cheap to run often if a refusal is durable. This walks the
/// whole path an adapter's sweep takes: screening on write, a refusal that
/// keeps its reason, a re-found candidate that changes nothing, and the operator
/// confirmation that is the only way a candidate becomes a target.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn candidates_are_screened_on_write_and_only_promote_when_confirmed()
-> Result<(), Box<dyn std::error::Error>> {
    let database_url =
        std::env::var("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").map_err(|error| {
            format!(
                "CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL must target a disposable database: {error}"
            )
        })?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;

    let workspace_id = WorkspaceId::new();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!(
            "discovery-e2e-{}",
            workspace_id.into_uuid().simple()
        ))
        .bind("Target discovery E2E")
        .execute(&pool)
        .await?;

    let database = DatabaseConfig {
        url: database_url,
        max_connections: 4,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(1),
    };
    let repository = PostgresAutopilotRepository::new(pool.clone(), &database);

    for (slug, cost) in [
        ("free-directory", ChannelCost::Free),
        ("placement-shop", ChannelCost::PaidPlacement),
    ] {
        repository
            .upsert_submission_channel(
                workspace_id,
                UpsertSubmissionChannel {
                    slug: slug.to_owned(),
                    display_name: slug.to_owned(),
                    cost_model: cost,
                    submission_url: None,
                    active: true,
                },
                &key(&format!("channel-{slug}")),
                None,
            )
            .await?;
    }

    // Writing the same channel again unchanged must not move its version: a
    // version that ticks on every sync is one nobody can reason about.
    let repeat = repository
        .upsert_submission_channel(
            workspace_id,
            UpsertSubmissionChannel {
                slug: "free-directory".to_owned(),
                display_name: "free-directory".to_owned(),
                cost_model: ChannelCost::Free,
                submission_url: None,
                active: true,
            },
            &key("channel-free-directory-again"),
            None,
        )
        .await?;
    assert_eq!(repeat.version, 1);

    let report = repository
        .ingest_outreach_candidates(
            workspace_id,
            vec![
                candidate("curator@example.test", RouteKind::Email, true, None),
                // Everything else about this one is excellent, which is exactly
                // when guessing an address is tempting.
                candidate("guessed@example.test", RouteKind::Email, false, None),
                candidate(
                    "shop@example.test",
                    RouteKind::Email,
                    true,
                    Some("placement-shop"),
                ),
                candidate(
                    "free@example.test",
                    RouteKind::Email,
                    true,
                    Some("free-directory"),
                ),
            ],
            &key("sweep-1"),
            None,
        )
        .await?;
    assert_eq!(report.received, 4);
    assert_eq!(report.admitted, 2);
    assert_eq!(report.refused, 2);
    assert_eq!(report.duplicates, 0);
    assert!(!report.replayed);

    assert_eq!(
        refusal(&pool, workspace_id, "guessed@example.test").await?,
        Some("route_inferred".to_owned())
    );
    assert_eq!(
        refusal(&pool, workspace_id, "shop@example.test").await?,
        Some("paid_placement".to_owned())
    );
    // A free channel is ordinary third-party contact; the paid one never gets a
    // class at all, because it never gets a pitch.
    assert_eq!(
        pitch_class(&pool, workspace_id, "free@example.test").await?,
        Some("third_party".to_owned())
    );

    // The same batch replayed is the same operation, not a second sweep.
    let replay = repository
        .ingest_outreach_candidates(
            workspace_id,
            vec![candidate(
                "curator@example.test",
                RouteKind::Email,
                true,
                None,
            )],
            &key("sweep-1"),
            None,
        )
        .await?;
    assert!(replay.replayed);
    assert_eq!(replay.admitted, 0);

    // Re-finding a candidate through another source is normal and must not
    // re-screen it.
    let refound = repository
        .ingest_outreach_candidates(
            workspace_id,
            vec![candidate(
                "curator@example.test",
                RouteKind::Email,
                true,
                None,
            )],
            &key("sweep-2"),
            None,
        )
        .await?;
    assert_eq!(refound.duplicates, 1);
    assert_eq!(refound.admitted, 0);

    let admitted = repository
        .list_outreach_candidates(workspace_id, Some("admitted".to_owned()), 50)
        .await?;
    assert_eq!(admitted.len(), 2);
    // The route never travels in the queue view.
    assert!(admitted.iter().all(|row| row.evidence.is_none()));

    let refused_id = candidate_id(&pool, workspace_id, "guessed@example.test").await?;
    let refused_confirm = repository
        .confirm_outreach_candidate(workspace_id, refused_id, &key("confirm-refused"), None)
        .await;
    assert!(matches!(refused_confirm, Err(RepositoryError::Conflict)));

    let admitted_id = candidate_id(&pool, workspace_id, "curator@example.test").await?;
    let promotion = repository
        .confirm_outreach_candidate(workspace_id, admitted_id, &key("confirm-1"), None)
        .await?;
    let target_id = promotion.target_id.ok_or("an email route must promote")?;

    let (contact_email, discovered_from) =
        sqlx::query_as::<_, (String, Option<Uuid>)>(
            "SELECT contact_email, discovered_from_candidate_id FROM viryaos_outreach_targets WHERE workspace_id=$1 AND id=$2",
        )
        .bind(workspace_id.into_uuid())
        .bind(target_id.into_uuid())
        .fetch_one(&pool)
        .await?;
    assert_eq!(contact_email, "curator@example.test");
    assert_eq!(discovered_from, Some(admitted_id));

    let status = sqlx::query_scalar::<_, String>(
        "SELECT status FROM viryaos_outreach_candidates WHERE workspace_id=$1 AND id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(admitted_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(status, "promoted");

    // A form route is a real published route with no pitcher yet: it must stay
    // a candidate rather than become a target with nowhere to put it.
    repository
        .ingest_outreach_candidates(
            workspace_id,
            vec![candidate(
                "https://curator.example.test/submit",
                RouteKind::SubmissionForm,
                true,
                None,
            )],
            &key("sweep-3"),
            None,
        )
        .await?;
    let form_id = candidate_id(&pool, workspace_id, "https://curator.example.test/submit").await?;
    let form_promotion = repository
        .confirm_outreach_candidate(workspace_id, form_id, &key("confirm-form"), None)
        .await?;
    assert!(form_promotion.target_id.is_none());

    // No cleanup: `operator_actions` is append-only by trigger, so deleting the
    // workspace would be refused by the very audit guarantee this ingress
    // relies on. The database this runs against is disposable by contract.
    Ok(())
}

/// Stable per logical operation, because that is the point: replaying "sweep-1"
/// must land on the same operator action. Idempotency is scoped per workspace
/// and each run gets a fresh one, so the keys need no run suffix.
fn key(value: &str) -> IdempotencyKey {
    IdempotencyKey::parse(format!("discovery-{value}")).expect("valid idempotency key")
}

fn candidate(
    route_value: &str,
    route: RouteKind,
    published: bool,
    channel_slug: Option<&str>,
) -> IngestOutreachCandidate {
    IngestOutreachCandidate {
        target_kind: OutreachTargetKind::Playlist,
        display_name: format!("Playlist {route_value}"),
        source: CandidateSource::PlaylistDescription,
        source_reference: "https://open.example.test/playlist/1".to_owned(),
        evidence: Some("Submissions: curator@example.test".to_owned()),
        route_kind: route,
        route_value: route_value.to_owned(),
        route_is_published: published,
        channel_slug: channel_slug.map(str::to_owned),
        fit_basis_points: 8_500,
        follower_count: Some(3_000),
        engagement_count: Some(200),
        sells_placement: false,
        churns_indiscriminately: false,
    }
}

async fn candidate_id(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    route_value: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM viryaos_outreach_candidates WHERE workspace_id=$1 AND route_value=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(route_value)
    .fetch_one(pool)
    .await?)
}

async fn refusal(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    route_value: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, Option<String>>(
        "SELECT refusal_reason FROM viryaos_outreach_candidates WHERE workspace_id=$1 AND route_value=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(route_value)
    .fetch_one(pool)
    .await?)
}

async fn pitch_class(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    route_value: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, Option<String>>(
        "SELECT pitch_class FROM viryaos_outreach_candidates WHERE workspace_id=$1 AND route_value=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(route_value)
    .fetch_one(pool)
    .await?)
}
