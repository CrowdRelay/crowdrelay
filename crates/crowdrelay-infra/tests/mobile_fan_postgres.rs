use std::time::Duration;

use crowdrelay_application::IdempotencyKey;
use crowdrelay_domain::{WorkspaceId, WorkspaceSlug};
use crowdrelay_infra::{
    config::DatabaseConfig,
    database,
    mobile_fan::{CityRequestCommand, MobileFanStoreError, PostgresMobileFanRepository},
};
use uuid::Uuid;

const TEST_DATABASE_URL_KEY: &str = "CROWDRELAY_MOBILE_FAN_TEST_DATABASE_URL";

#[tokio::test]
#[ignore = "requires CROWDRELAY_MOBILE_FAN_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn city_request_idempotency_counts_each_client_operation_once()
-> Result<(), Box<dyn std::error::Error>> {
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
