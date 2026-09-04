use std::time::Duration;

use crowdrelay_application::IdempotencyKey;
use crowdrelay_domain::{WorkspaceId, WorkspaceSlug};
use crowdrelay_infra::{
    config::DatabaseConfig,
    database,
    mobile_fan::{CityRequestCommand, MobileFanStoreError, PostgresMobileFanRepository},
};
use sqlx::PgPool;
use uuid::Uuid;

const TEST_DATABASE_URL_KEY: &str = "CROWDRELAY_MOBILE_FAN_TEST_DATABASE_URL";

async fn test_pool() -> Result<PgPool, Box<dyn std::error::Error>> {
    let database_url = std::env::var(TEST_DATABASE_URL_KEY)
        .map_err(|error| format!("set {TEST_DATABASE_URL_KEY}: {error}"))?;
    let config = DatabaseConfig {
        url: database_url,
        max_connections: 4,
        connect_timeout: Duration::from_secs(5),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(2),
    };
    let pool = database::connect(&config).await?;
    database::migrate(&pool).await?;
    Ok(pool)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_MOBILE_FAN_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn city_request_idempotency_counts_each_client_operation_once()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = test_pool().await?;

    let suffix = Uuid::now_v7().simple().to_string();
    let workspace_id = WorkspaceId::new();
    let workspace_slug = WorkspaceSlug::parse(format!("mobile-{suffix}"))?;
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Mobile fan test')")
        .bind(workspace_id.into_uuid())
        .bind(workspace_slug.as_str())
        .execute(&pool)
        .await?;

    let repository =
        PostgresMobileFanRepository::new(pool.clone(), workspace_id, Duration::from_secs(5));
    let key = IdempotencyKey::parse(format!("city-request-{suffix}"))?;
    let command = CityRequestCommand {
        idempotency_key: key.clone(),
        request_id: Some(format!("request-{suffix}")),
        name: "Bielawa".to_owned(),
        region: Some("Dolnoslaskie".to_owned()),
        country_code: "PL".to_owned(),
        slug: format!("pending-bielawa-{suffix}"),
    };

    let first = repository.request_city(&command).await?;
    let replay = repository.request_city(&command).await?;
    assert_eq!(first, replay);
    assert_eq!(first.status, "pending");

    let count = sqlx::query_scalar::<_, i32>("SELECT request_count FROM cities WHERE slug = $1")
        .bind(&command.slug)
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 1, "exact replay must not inflate city demand");

    let changed = CityRequestCommand {
        idempotency_key: key,
        request_id: Some(format!("request-changed-{suffix}")),
        name: "Dzierzoniow".to_owned(),
        region: command.region.clone(),
        country_code: command.country_code.clone(),
        slug: format!("pending-dzierzoniow-{suffix}"),
    };
    assert_eq!(
        repository.request_city(&changed).await,
        Err(MobileFanStoreError::Conflict),
    );

    let second = CityRequestCommand {
        idempotency_key: IdempotencyKey::parse(format!("city-request-second-{suffix}"))?,
        request_id: Some(format!("request-second-{suffix}")),
        ..command
    };
    repository.request_city(&second).await?;
    let count = sqlx::query_scalar::<_, i32>("SELECT request_count FROM cities WHERE slug = $1")
        .bind(&second.slug)
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 2, "a distinct client operation must still count");

    let stored = sqlx::query_as::<_, (String, i32)>(
        "SELECT state, response_status FROM idempotency_keys WHERE workspace_id = $1 AND scope = 'city_request' AND key = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind(second.idempotency_key.as_str())
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored.0, "completed");
    assert_eq!(stored.1, 202);
    Ok(())
}

/// Proves the nearby-gig statement against the real schema, and pins the two
/// properties the radius filter has to hold.
///
/// This path had no live coverage at all, which matters more here than usual:
/// the workspace runs its SQL through `sqlx::query` rather than the macros, so
/// nothing before this test executed the statement against a real database. It
/// is also the only automatic reason an installed app reopens on its own, so a
/// silent break here costs attendance rather than an error page.
///
/// The candidate scan now rejects a pair on latitude alone before spending a
/// haversine on it, and skips any pair already notified. Both must be
/// conservative: the boundary show below sits 149 km from the fan against a
/// 150 km radius, close enough to the cut that a prefilter even slightly too
/// tight would drop it and the assertion would fail.
#[tokio::test]
#[ignore = "requires CROWDRELAY_MOBILE_FAN_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn nearby_gigs_notify_inside_the_radius_once_and_never_outside_it()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = test_pool().await?;
    let suffix = Uuid::now_v7().simple().to_string();
    let workspace_id = WorkspaceId::new();
    let workspace_slug = WorkspaceSlug::parse(format!("nearby-{suffix}"))?;
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Nearby gig test')")
        .bind(workspace_id.into_uuid())
        .bind(workspace_slug.as_str())
        .execute(&pool)
        .await?;

    // One degree of latitude is 111.19 km, so a shared longitude turns the
    // offsets below into exact distances: 55.6 km, 149.0 km and 444.8 km.
    let city = async |slug: &str, latitude: f64| -> Result<Uuid, sqlx::Error> {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO cities (slug, name, country_code, latitude, longitude)
             VALUES ($1, $2, 'PL', $3, 21.0) RETURNING id",
        )
        .bind(format!("{slug}-{suffix}"))
        .bind(slug)
        .bind(latitude)
        .fetch_one(&pool)
        .await
    };
    let home_city = city("home", 52.0).await?;
    let near_city = city("near", 52.5).await?;
    let boundary_city = city("boundary", 53.34).await?;
    let distant_city = city("distant", 56.0).await?;

    let fan_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO fans (workspace_id, normalized_email, locale, status)
         VALUES ($1, $2, 'pl-PL', 'active') RETURNING id",
    )
    .bind(workspace_id.into_uuid())
    .bind(format!("nearby-{suffix}@example.test"))
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_location_preferences
             (workspace_id, fan_id, city_id, nearby_gigs_enabled, radius_km)
         VALUES ($1, $2, $3, true, 150)",
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id)
    .bind(home_city)
    .execute(&pool)
    .await?;

    let event = async |slug: &str, city_id: Uuid| -> Result<Uuid, sqlx::Error> {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO events
                 (workspace_id, city_id, slug, title, starts_at, status, published_at)
             VALUES ($1, $2, $3, $4, now() + interval '30 days', 'published', now())
             RETURNING id",
        )
        .bind(workspace_id.into_uuid())
        .bind(city_id)
        .bind(format!("{slug}-{suffix}"))
        .bind(slug)
        .fetch_one(&pool)
        .await
    };
    let near_event = event("near", near_city).await?;
    let boundary_event = event("boundary", boundary_city).await?;
    let distant_event = event("distant", distant_city).await?;

    let repository =
        PostgresMobileFanRepository::new(pool.clone(), workspace_id, Duration::from_secs(5));
    let (queued, push_queued) = repository
        .emit_due_nearby_gigs(Some(&format!("request-{suffix}")), false)
        .await?;
    assert_eq!(queued, 2, "both shows inside the radius must be announced");
    assert_eq!(push_queued, 0, "push was disabled for this run");

    let notified = sqlx::query_scalar::<_, Uuid>(
        "SELECT event_id FROM nearby_gig_notifications
         WHERE workspace_id = $1 AND fan_id = $2 ORDER BY event_id",
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id)
    .fetch_all(&pool)
    .await?;
    let mut expected = vec![near_event, boundary_event];
    expected.sort_unstable();
    assert_eq!(
        notified, expected,
        "the 149 km show is inside a 150 km radius and must survive the latitude prefilter"
    );
    assert!(
        !notified.contains(&distant_event),
        "a show 445 km away must never be announced against a 150 km radius"
    );

    let (queued_again, _) = repository
        .emit_due_nearby_gigs(Some(&format!("request-repeat-{suffix}")), false)
        .await?;
    assert_eq!(
        queued_again, 0,
        "a fan already told about a show must not be told again on the next run"
    );
    Ok(())
}
