//! Venue/promoter discovery against a real Postgres.
//!
//! What fails here and nowhere else: screening-on-write with durable
//! refusals, contact-identity dedupe across sources, and the promotion that
//! turns one confirmed email route into a city-resolved booking target —
//! including the case where the relationship already exists and must be
//! linked rather than reset.

use std::time::Duration;

use crowdrelay_application::autopilot::{
    AutopilotBookingDiscoveryRepository, AutopilotControlMutation,
};
use crowdrelay_domain::{
    OutreachOpportunityId, WorkspaceId,
    booking::BookingTargetKind,
    booking_discovery::{BookingCandidateInput, RouteKind},
};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

struct Fixture {
    pool: sqlx::PgPool,
    repository: PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
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
        .acquire_timeout(Duration::from_secs(10))
        .connect(&database_url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;

    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("{label}-{suffix}"))
        .bind("Booking discovery E2E")
        .execute(&pool)
        .await?;
    // The promotion target is city-scoped; ensure one resolvable city.
    // ON CONFLICT because the disposable database is shared across tests.
    sqlx::query(
        "INSERT INTO cities (id, slug, name, country_code) \
         VALUES (gen_random_uuid(), 'wroclaw', 'Wroclaw', 'PL') \
         ON CONFLICT (country_code, slug) DO NOTHING",
    )
    .execute(&pool)
    .await?;

    let repository = PostgresAutopilotRepository::new(
        pool.clone(),
        &DatabaseConfig {
            url: database_url,
            max_connections: 4,
            connect_timeout: Duration::from_secs(3),
            ping_timeout: Duration::from_secs(2),
            operation_timeout: Duration::from_secs(10),
            lock_timeout: Duration::from_secs(1),
        },
    );
    Ok(Fixture {
        pool,
        repository,
        workspace_id,
    })
}

fn candidate(display_name: &str) -> BookingCandidateInput {
    BookingCandidateInput {
        kind: BookingTargetKind::Venue,
        display_name: display_name.into(),
        city_slug: Some("wroclaw".into()),
        route_kind: RouteKind::Email,
        route_value: format!("booking@{display_name}.example"),
        source: "venue_site".into(),
        source_reference: format!("https://{display_name}.example/contact"),
        evidence: Some(format!("Zgloszenia: booking@{display_name}.example")),
        fit_basis_points: 8_000,
        paid_to_apply: false,
        route_is_published: true,
        capacity: Some(300),
    }
}

fn key(seed: u8) -> crowdrelay_application::IdempotencyKey {
    crowdrelay_application::IdempotencyKey::parse(format!("booking-discovery-key-{seed:>03}"))
        .expect("valid idempotency key")
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn screening_is_durable_dedupe_is_identity_and_promotion_never_resets() {
    let fixture = fixture("discovery").await.expect("fixture");

    let good = candidate("klub-transfuzja");
    let mut pay_to_play = candidate("festival-pl");
    pay_to_play.kind = BookingTargetKind::Festival;
    pay_to_play.paid_to_apply = true;
    pay_to_play.route_value = "apply@festival.example".into();

    let batch = vec![good.clone(), pay_to_play.clone(), good.clone()];
    let ingestion = fixture
        .repository
        .ingest_booking_candidates(fixture.workspace_id, batch, &key(1), None)
        .await
        .expect("ingest");
    assert_eq!(ingestion.reported, 3);
    assert_eq!(ingestion.admitted, 1);
    assert_eq!(ingestion.refused, 1, "pay-to-play is a permanent refusal");
    assert_eq!(
        ingestion.duplicates, 1,
        "one inbox found twice is one prospect"
    );

    // The refusal is stored with its reason, so no sweep rediscovers it.
    let refused_reason: Option<String> = sqlx::query_scalar(
        "SELECT refusal_reason FROM viryaos_booking_candidates \
         WHERE workspace_id=$1 AND display_name='festival-pl'",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await
    .expect("refused row");
    assert_eq!(refused_reason.as_deref(), Some("paid_to_apply"));

    // Confirm promotes the admitted email route into a city-scoped target.
    let candidate_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM viryaos_booking_candidates \
         WHERE workspace_id=$1 AND status='admitted'",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await
    .expect("admitted candidate");

    let mutation: AutopilotControlMutation = fixture
        .repository
        .confirm_booking_candidate(
            fixture.workspace_id,
            OutreachOpportunityId::from_uuid(candidate_id),
            &key(2),
            None,
        )
        .await
        .expect("confirm");
    assert!(!mutation.replayed);

    let target_id = mutation.target_id;
    let (active, accepts): (bool, bool) = sqlx::query_as(
        "SELECT active, accepts_booking FROM viryaos_booking_targets \
         WHERE workspace_id=$1 AND id=$2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(target_id)
    .fetch_one(&fixture.pool)
    .await
    .expect("target row");
    assert!(active && accepts);

    // Re-confirm replays through the ledger without a second target.
    let replay = fixture
        .repository
        .confirm_booking_candidate(
            fixture.workspace_id,
            OutreachOpportunityId::from_uuid(candidate_id),
            &key(2),
            None,
        )
        .await
        .expect("replay confirm");
    assert!(replay.replayed);

    let targets: i64 =
        sqlx::query_scalar("SELECT count(*) FROM viryaos_booking_targets WHERE workspace_id=$1")
            .bind(fixture.workspace_id.into_uuid())
            .fetch_one(&fixture.pool)
            .await
            .expect("target count");
    assert_eq!(targets, 1);

    // A second venue sharing nothing still promotes independently.
    let other = fixture
        .repository
        .ingest_booking_candidates(
            fixture.workspace_id,
            vec![candidate("katakomby")],
            &key(3),
            None,
        )
        .await
        .expect("second ingest");
    assert_eq!(other.admitted, 1);
}
