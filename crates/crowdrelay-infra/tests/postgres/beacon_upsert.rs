//! Beacon upsert identity — the P.3 admit path's dedupe and consent rules.
//!
//! A name-only admit creates a beacon with no contact identity; the natural
//! match must still find it on a second admit (no duplicate stubs) and a
//! later create carrying an email must adopt it rather than insert a second
//! row beside it. And a create-intent that collides with a real contact must
//! never re-arm its consent flags — suppression is a record, not a default a
//! resubmit resets.

use std::time::Duration;

use crate::common;
use crowdrelay_application::IdempotencyKey;
use crowdrelay_application::autopilot::{AutopilotBeaconStateRepository, UpsertBeacon};
use crowdrelay_domain::beacons::BeaconKind;
use crowdrelay_domain::{WorkspaceId, autonomy::Confidence};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use uuid::Uuid;

async fn repository()
-> Result<(PostgresAutopilotRepository, sqlx::PgPool), Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let database = DatabaseConfig {
        url: database_url.clone(),
        max_connections: 4,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(1),
    };
    Ok((
        PostgresAutopilotRepository::new(pool.clone(), &database),
        pool,
    ))
}

fn admit(name: &str, city_id: Option<Uuid>) -> UpsertBeacon {
    UpsertBeacon {
        beacon_id: None,
        city_id: city_id.map(crowdrelay_domain::CityId::from_uuid),
        kind: BeaconKind::ScenePartner,
        display_name: name.to_owned(),
        contact_email: None,
        destination_url: None,
        source_url: None,
        active: true,
        verified: false,
        accepts_outreach: false,
        do_not_contact: false,
        relationship_score: 50,
        relevance_basis_points: 7_500,
        confidence: Confidence::saturating_from_basis_points(7_500),
        metadata: serde_json::json!({}),
        expected_version: 0,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_name_only_admit_dedupes_and_a_later_email_adopts_the_stub()
-> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::from_uuid(Uuid::now_v7());
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Beacon Test')")
        .bind(workspace_id.into_uuid())
        .bind(format!("ws-{}", workspace_id.into_uuid().simple()))
        .execute(&pool)
        .await?;
    let city_id: Uuid = sqlx::query_scalar(
        "INSERT INTO cities (slug, name, country_code, latitude, longitude)
         VALUES ('wroclaw-beacon', 'Wrocław', 'PL', 51.1, 17.03)
         ON CONFLICT (country_code, slug) DO UPDATE SET name = EXCLUDED.name
         RETURNING id",
    )
    .fetch_one(&pool)
    .await?;

    // The first admit inserts the stub.
    let first = repo
        .upsert_beacon(
            workspace_id,
            admit("The Openers", Some(city_id)),
            &IdempotencyKey::parse("admit-01")?,
            None,
        )
        .await?;
    assert!(!first.replayed);

    // A second admit of the same name finds the stub — same beacon, no dupe.
    let second = repo
        .upsert_beacon(
            workspace_id,
            admit("The Openers", Some(city_id)),
            &IdempotencyKey::parse("admit-02")?,
            None,
        )
        .await?;
    assert_eq!(
        second.beacon_id, first.beacon_id,
        "a re-admit of the same name minted a second beacon"
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM viryaos_beacons WHERE workspace_id = $1")
            .bind(workspace_id.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(count, 1, "the name-stub match must dedupe");

    // A different name in the same city/kind is a different band — both live.
    let other = repo
        .upsert_beacon(
            workspace_id,
            admit("Different Band", Some(city_id)),
            &IdempotencyKey::parse("admit-03")?,
            None,
        )
        .await?;
    assert_ne!(other.beacon_id, first.beacon_id);

    // The operator finds the channel later: a create carrying the email
    // adopts the stub — same row, identity filled, outreach armed by the
    // operator's own assertion.
    let mut enrich = admit("the openers", Some(city_id)); // case differs on purpose
    enrich.contact_email = Some("openers@example.com".to_owned());
    enrich.verified = true;
    enrich.accepts_outreach = true;
    let adopted = repo
        .upsert_beacon(
            workspace_id,
            enrich,
            &IdempotencyKey::parse("admit-04")?,
            None,
        )
        .await?;
    assert_eq!(
        adopted.beacon_id, first.beacon_id,
        "a create carrying the email must adopt the stub, not duplicate it"
    );
    let row: (bool, bool, Option<String>) = sqlx::query_as(
        "SELECT verified, accepts_outreach, contact_email
         FROM viryaos_beacons WHERE id = $1",
    )
    .bind(first.beacon_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(row.2.as_deref(), Some("openers@example.com"));
    assert!(
        row.0 && row.1,
        "adoption is the enrich step that arms the row"
    );

    // A create-intent colliding with a REAL contact (email match) must not
    // re-arm a suppression the operator already recorded.
    sqlx::query(
        "UPDATE viryaos_beacons
         SET do_not_contact = true, accepts_outreach = false, verified = false
         WHERE id = $1",
    )
    .bind(first.beacon_id.into_uuid())
    .execute(&pool)
    .await?;
    let mut resubmit = admit("The Openers", Some(city_id));
    resubmit.contact_email = Some("openers@example.com".to_owned());
    resubmit.verified = true;
    resubmit.accepts_outreach = true;
    let suppressed = repo
        .upsert_beacon(
            workspace_id,
            resubmit,
            &IdempotencyKey::parse("admit-05")?,
            None,
        )
        .await?;
    assert_eq!(suppressed.beacon_id, first.beacon_id);
    let (dnc, outreach, verified): (bool, bool, bool) = sqlx::query_as(
        "SELECT do_not_contact, accepts_outreach, verified
         FROM viryaos_beacons WHERE id = $1",
    )
    .bind(first.beacon_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert!(
        dnc,
        "a create-intent must never clear a recorded suppression"
    );
    assert!(
        !outreach,
        "a duplicate create must not re-arm outreach on a real row"
    );
    assert!(
        !verified,
        "a duplicate create must not re-verify a suppressed row"
    );

    Ok(())
}
