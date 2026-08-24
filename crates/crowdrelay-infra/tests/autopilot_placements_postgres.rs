//! Placement verification against a real Postgres.
//!
//! The domain is unit-tested and the state machine is not what fails here. What
//! fails here is the wiring, and the two statements that matter both do
//! something no type checks: a conditional update that must not advance a
//! checkpoint on a failed read, and a suppression that has to reach every
//! target one curator runs.
//!
//! If the second one breaks, the agent pitches a known scammer again next week
//! through a different playlist and nothing anywhere says so.

use std::time::Duration;

use crowdrelay_application::autopilot::{
    AutopilotDecisionRepository, AutopilotTeamStateRepository, RecordPlaylistPlacement,
};
use crowdrelay_application::{IdempotencyKey, RepositoryError};
use crowdrelay_domain::{
    OutreachOpportunityId, WorkspaceId,
    playlist_placement::{PlacementObservation, PlacementState},
};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::postgres::PgPoolOptions;
use time::OffsetDateTime;
use uuid::Uuid;

struct Fixture {
    pool: sqlx::PgPool,
    repository: PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    /// The playlist the claim is about, and a second one the same curator runs.
    opportunity_id: OutreachOpportunityId,
    sibling_target: Uuid,
    stranger_target: Uuid,
    now: OffsetDateTime,
}

async fn insert_target(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    label: &str,
    identity: Option<&str>,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO viryaos_outreach_targets (
             workspace_id, target_kind, display_name, contact_email,
             active, verified, accepts_outreach, curator_identity
         ) VALUES ($1,'playlist',$2,$3,true,true,true,$4)
         RETURNING id",
    )
    .bind(workspace_id.into_uuid())
    .bind(label)
    .bind(format!("{label}@example.test"))
    .bind(identity)
    .fetch_one(pool)
    .await?)
}

async fn fixture(label: &str) -> Result<Fixture, Box<dyn std::error::Error>> {
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
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("{label}-{suffix}"))
        .bind("Placements E2E")
        .execute(&pool)
        .await?;
    let now = OffsetDateTime::now_utc();
    let identity = format!("curator-{suffix}");
    // Two playlists run by one person, and one run by somebody else.
    let pitched =
        insert_target(&pool, workspace_id, &format!("a-{suffix}"), Some(&identity)).await?;
    let sibling_target =
        insert_target(&pool, workspace_id, &format!("b-{suffix}"), Some(&identity)).await?;
    let stranger_target = insert_target(&pool, workspace_id, &format!("c-{suffix}"), None).await?;

    let opportunity_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO viryaos_outreach_opportunities (
             workspace_id, target_id, source, subject_kind, subject_key, template_key,
             relevance_basis_points, confidence_basis_points, observed_at, expires_at
         ) VALUES ($1,$2,'manual','release',$3,'outreach.playlist.v1',9000,9000,$4,$5)
         RETURNING id",
    )
    .bind(workspace_id.into_uuid())
    .bind(pitched)
    .bind(format!("release-{suffix}"))
    .bind(now)
    .bind(now + time::Duration::days(60))
    .fetch_one(&pool)
    .await?;
    // A second opportunity on the sibling playlist, so the suppression has
    // something to close that is not the one the claim was about.
    sqlx::query(
        "INSERT INTO viryaos_outreach_opportunities (
             workspace_id, target_id, source, subject_kind, subject_key, template_key,
             relevance_basis_points, confidence_basis_points, observed_at, expires_at
         ) VALUES ($1,$2,'manual','release',$3,'outreach.playlist.v1',9000,9000,$4,$5)",
    )
    .bind(workspace_id.into_uuid())
    .bind(sibling_target)
    .bind(format!("release-sibling-{suffix}"))
    .bind(now)
    .bind(now + time::Duration::days(60))
    .execute(&pool)
    .await?;

    let database = DatabaseConfig {
        url: database_url,
        max_connections: 4,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(10),
        lock_timeout: Duration::from_secs(1),
    };
    let repository = PostgresAutopilotRepository::new(pool.clone(), &database);
    Ok(Fixture {
        pool,
        repository,
        workspace_id,
        opportunity_id: OutreachOpportunityId::from_uuid(opportunity_id),
        sibling_target,
        stranger_target,
        now,
    })
}

async fn record(
    fixture: &Fixture,
    observation: Option<PlacementObservation>,
    key: &str,
) -> Result<String, RepositoryError> {
    fixture
        .repository
        .record_playlist_placement(
            fixture.workspace_id,
            RecordPlaylistPlacement {
                opportunity_id: fixture.opportunity_id,
                playlist_external_id: "spotify:playlist:probe".to_owned(),
                track_external_id: "spotify:track:probe".to_owned(),
                observation,
            },
            &IdempotencyKey::parse(format!("placement-probe-{key}")).expect("valid key"),
            None,
        )
        .await
        .map(|mutation| mutation.status)
}

async fn stored(
    fixture: &Fixture,
) -> Result<(String, i16, Option<String>), Box<dyn std::error::Error>> {
    Ok(sqlx::query_as::<_, (String, i16, Option<String>)>(
        "SELECT state, checks_completed, last_observation FROM viryaos_playlist_placements
         WHERE workspace_id=$1 AND opportunity_id=$2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fixture.opportunity_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await?)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_failed_read_advances_nothing_and_a_real_one_advances_once()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("placement-reads").await?;
    assert_eq!(record(&fixture, None, "claim").await?, "claimed");
    assert_eq!(stored(&fixture).await?, ("claimed".to_owned(), 0, None));
    // A curator who claims twice is claiming about the same thing.
    assert!(matches!(
        record(&fixture, None, "claim-again").await,
        Err(RepositoryError::Conflict)
    ));

    // A dead credential is not evidence that a track is gone.
    assert_eq!(
        record(&fixture, Some(PlacementObservation::Unreadable), "dead").await?,
        "claimed"
    );
    assert_eq!(
        stored(&fixture).await?,
        ("claimed".to_owned(), 0, None),
        "an unreadable check settles nothing and consumes no checkpoint"
    );

    assert_eq!(
        record(&fixture, Some(PlacementObservation::Present), "seen").await?,
        "verified"
    );
    assert_eq!(
        stored(&fixture).await?,
        ("verified".to_owned(), 1, Some("present".to_owned()))
    );
    // The cycle still owns it: verified is not settled.
    assert!(!PlacementState::Verified.settled());
    assert_eq!(
        fixture
            .repository
            .load_playlist_placements(fixture.workspace_id, fixture.now)
            .await?
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_withdrawal_suppresses_every_playlist_that_curator_runs()
-> Result<(), Box<dyn std::error::Error>> {
    // The test worth keeping. Suppress only the playlist the track was pulled
    // from and the same person is pitched next week under another name.
    let fixture = fixture("placement-withdrawn").await?;
    record(&fixture, None, "claim").await?;
    record(&fixture, Some(PlacementObservation::Present), "seen").await?;
    assert_eq!(
        record(&fixture, Some(PlacementObservation::Absent), "gone").await?,
        "withdrawn"
    );
    assert_eq!(
        stored(&fixture).await?.0,
        "withdrawn",
        "confirmed and then gone inside the window"
    );

    let suppressed = sqlx::query_as::<_, (Uuid, bool, bool)>(
        "SELECT id, do_not_contact, accepts_outreach FROM viryaos_outreach_targets
         WHERE workspace_id=$1 ORDER BY id",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_all(&fixture.pool)
    .await?;
    for (id, do_not_contact, accepts) in suppressed {
        if id == fixture.stranger_target {
            assert!(!do_not_contact, "somebody else's playlist is untouched");
            assert!(accepts);
        } else {
            assert!(
                do_not_contact,
                "both playlists this curator runs are suppressed, including {id}"
            );
            assert!(!accepts);
        }
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM viryaos_outreach_opportunities AS opportunity
             JOIN viryaos_outreach_targets AS target
               ON target.workspace_id=opportunity.workspace_id AND target.id=opportunity.target_id
             WHERE opportunity.workspace_id=$1 AND opportunity.active AND target.do_not_contact"
        )
        .bind(fixture.workspace_id.into_uuid())
        .fetch_one(&fixture.pool)
        .await?,
        0,
        "their open opportunities go with them, or the next cycle pitches them anyway"
    );
    assert_ne!(fixture.sibling_target, fixture.stranger_target);

    // Settled, so the cycle never reads it again.
    assert!(
        fixture
            .repository
            .load_playlist_placements(fixture.workspace_id, fixture.now)
            .await?
            .is_empty()
    );
    Ok(())
}
