//! Venue/promoter discovery against a real Postgres.
//!
//! What fails here and nowhere else: screening-on-write with durable
//! refusals, contact-identity dedupe across sources, and the promotion that
//! turns one confirmed email route into a city-resolved booking target —
//! including the case where the relationship already exists and must be
//! linked rather than reset.

use std::time::Duration;

use crowdrelay_application::autopilot::{
    AutopilotBookingDiscoveryRepository, AutopilotContext, AutopilotControlMutation,
    EvaluateAutopilot,
};
use crowdrelay_domain::{
    OutreachOpportunityId, WorkspaceId,
    booking::BookingTargetKind,
    booking_discovery::{BookingCandidateInput, RouteKind},
};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::postgres::PgPoolOptions;
use time::OffsetDateTime;
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

/// A candidate whose city is not in the 22-row city table must still promote:
/// the confirm mints a deterministic `pending-*` row rather than failing on
/// geography, and a second candidate for the same place lands on it.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_unresolved_city_mints_a_pending_row_on_confirm() {
    let fixture = fixture("pending-city").await.expect("fixture");

    let mut berlin = candidate("club-berghain");
    berlin.city_slug = Some("berlin".into());
    let mut halle = candidate("halle-tor");
    halle.city_slug = Some("berlin".into());

    fixture
        .repository
        .ingest_booking_candidates(fixture.workspace_id, vec![berlin], &key(4), None)
        .await
        .expect("first ingest");
    fixture
        .repository
        .ingest_booking_candidates(fixture.workspace_id, vec![halle], &key(5), None)
        .await
        .expect("second ingest");

    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM viryaos_booking_candidates \
         WHERE workspace_id=$1 AND status='admitted' ORDER BY display_name",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_all(&fixture.pool)
    .await
    .expect("admitted candidates");
    assert_eq!(ids.len(), 2);

    let first = fixture
        .repository
        .confirm_booking_candidate(
            fixture.workspace_id,
            OutreachOpportunityId::from_uuid(ids[0]),
            &key(6),
            None,
        )
        .await
        .expect("first confirm must not fail on an unknown city");

    let (slug, status, country): (String, String, String) = sqlx::query_as(
        "SELECT slug, moderation_status, country_code FROM cities \
         WHERE id = (SELECT city_id FROM viryaos_booking_targets WHERE id = $1)",
    )
    .bind(first.target_id)
    .fetch_one(&fixture.pool)
    .await
    .expect("pending city row");
    assert!(
        slug.starts_with("pending-"),
        "unknown city mints a pending row"
    );
    assert_eq!(status, "pending");
    assert_eq!(country, "XX", "country is honestly unknown, never guessed");

    // The second candidate for the same place resolves to the same minted
    // row — deterministic slugs keep promotion idempotent.
    let second = fixture
        .repository
        .confirm_booking_candidate(
            fixture.workspace_id,
            OutreachOpportunityId::from_uuid(ids[1]),
            &key(7),
            None,
        )
        .await
        .expect("second confirm");
    let (city_a, city_b): (Uuid, Uuid) = sqlx::query_as(
        "SELECT \
           (SELECT city_id FROM viryaos_booking_targets WHERE id = $1), \
           (SELECT city_id FROM viryaos_booking_targets WHERE id = $2)",
    )
    .bind(first.target_id)
    .bind(second.target_id)
    .fetch_one(&fixture.pool)
    .await
    .expect("city ids");
    assert_eq!(city_a, city_b, "same source city, same pending row");

    // A separator-only slug has nothing to title-case — the source still
    // named a place, so confirmation must mint a row from the raw slug
    // rather than fail forever on a place nobody can resolve.
    let mut noise = candidate("club-signals");
    noise.city_slug = Some("---".into());
    fixture
        .repository
        .ingest_booking_candidates(fixture.workspace_id, vec![noise], &key(8), None)
        .await
        .expect("noise ingest");
    let noise_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM viryaos_booking_candidates \
         WHERE workspace_id=$1 AND city_slug='---' AND status='admitted'",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await
    .expect("noise candidate");
    let noise_confirm = fixture
        .repository
        .confirm_booking_candidate(
            fixture.workspace_id,
            OutreachOpportunityId::from_uuid(noise_id),
            &key(9),
            None,
        )
        .await
        .expect("separator-only slug must still promote");
    let noise_slug: String = sqlx::query_scalar(
        "SELECT slug FROM cities WHERE id = \
         (SELECT city_id FROM viryaos_booking_targets WHERE id = $1)",
    )
    .bind(noise_confirm.target_id)
    .fetch_one(&fixture.pool)
    .await
    .expect("noise city row");
    assert!(
        noise_slug.starts_with("pending-"),
        "raw-slug fallback still mints a pending row, got {noise_slug}"
    );
    assert_ne!(
        noise_slug, slug,
        "a different source slug lands on a different pending row"
    );
}

/// Prod shape: agent live, an enabled `booking_opportunity` policy under
/// `require_approval`, zero booking targets. The supply alarm must produce a
/// discovery request — on production it never did, with no error and no row.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_starved_booking_pipeline_requests_target_discovery() {
    let fixture = fixture("supply").await.expect("fixture");

    sqlx::query(
        "INSERT INTO viryaos_growth_envelope (workspace_id, agent_enabled, dry_run) \
         VALUES ($1, true, false) \
         ON CONFLICT (workspace_id) DO UPDATE SET agent_enabled = true, dry_run = false",
    )
    .bind(fixture.workspace_id.into_uuid())
    .execute(&fixture.pool)
    .await
    .expect("envelope");
    sqlx::query(
        "INSERT INTO viryaos_autopilot_policies \
         (workspace_id, context, enabled, autonomy_level, \
          minimum_confidence_basis_points, max_actions_24h) \
         VALUES ($1, 'booking_opportunity', true, 'require_approval', 8000, 10) \
         ON CONFLICT (workspace_id, context) DO UPDATE SET \
         enabled = true, autonomy_level = 'require_approval', \
         minimum_confidence_basis_points = 8000, max_actions_24h = 10",
    )
    .bind(fixture.workspace_id.into_uuid())
    .execute(&fixture.pool)
    .await
    .expect("policy");

    let report = EvaluateAutopilot::new(&fixture.repository, fixture.workspace_id)
        .execute(OffsetDateTime::now_utc())
        .await
        .expect("evaluate");

    let decisions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM viryaos_autopilot_decisions \
         WHERE workspace_id=$1 AND decision_kind='request_booking_target_discovery'",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await
    .expect("decision count");
    assert_eq!(
        decisions, 1,
        "zero targets under an enabled booking policy must ask for discovery \
         (report: {report:?})"
    );
    let actions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM viryaos_autopilot_actions \
         WHERE workspace_id=$1 AND action_kind='booking.target_discovery.request'",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await
    .expect("action count");
    assert_eq!(
        actions, 1,
        "the request must leave an action an executor can claim"
    );

    // The cycle report must say where the work came from — the silence this
    // regresses against read as a cycle with decisions from every context
    // except booking, and nothing in the report could say so.
    let booking = report
        .context_activity
        .get(&AutopilotContext::BookingOpportunity)
        .expect("the booking arm ran");
    assert_eq!(booking.candidates, 1);
    assert_eq!(booking.decisions, 1);
    assert_eq!(booking.actions, 1);
    assert_eq!(booking.throttled, 0);
}
