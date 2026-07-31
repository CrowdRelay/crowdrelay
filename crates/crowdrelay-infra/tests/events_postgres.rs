use std::time::Duration;

use crowdrelay_application::{
    AcquisitionRepository, EventRepository, IdempotencyKey, RegisterEventInterestCommand,
    RequestId, SignupFanCommand,
};
use crowdrelay_domain::{
    CitySlug, CountryCode, EventAction, EventActionKind, EventId, EventSlug, FanSignup,
    FanSignupInput, MarketingConsent, NormalizedEmail, VisitorId, WorkspaceId, WorkspaceSlug,
};
use crowdrelay_infra::{
    acquisition::PostgresAcquisitionRepository,
    config::DatabaseConfig,
    events::PostgresEventRepository,
    sensitive_response::{SensitiveResponseCodec, SensitiveResponseKey},
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use time::OffsetDateTime;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires CROWDRELAY_EVENT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn publishes_events_tracks_actions_and_registers_interest_idempotently()
-> Result<(), Box<dyn std::error::Error>> {
    let database_url = std::env::var("CROWDRELAY_EVENT_TEST_DATABASE_URL").map_err(|e| {
        format!("CROWDRELAY_EVENT_TEST_DATABASE_URL must target a disposable database: {e}")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&database_url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;

    let workspace_id = WorkspaceId::new();
    let workspace_slug =
        WorkspaceSlug::parse(format!("event-e2e-{}", workspace_id.into_uuid().simple()))?;
    let starts_at = OffsetDateTime::now_utc() + time::Duration::days(2);
    let event_id = seed_fixture(&pool, workspace_id, &workspace_slug, starts_at).await?;

    let database = DatabaseConfig {
        url: database_url,
        max_connections: 8,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(1),
    };
    let acquisition = PostgresAcquisitionRepository::new(
        pool.clone(),
        workspace_slug.clone(),
        CountryCode::parse("PL")?,
        &database,
        false,
        test_sensitive_response_codec(),
    );
    let events =
        PostgresEventRepository::new(pool.clone(), workspace_slug, &database, vec![1_440, 120]);

    let fan = acquisition
        .persist_fan_signup(&signup_command(workspace_id)?)
        .await?;

    let published = events.load_published_events().await?;
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].id.into_uuid(), event_id);
    assert_eq!(published[0].slug.as_str(), "wroclaw-live-2026");

    let command = RegisterEventInterestCommand::new(
        crowdrelay_application::RegisterEventInterestCommandArgs {
            workspace_id,
            event_slug: EventSlug::parse("wroclaw-live-2026")?,
            fan_session: fan.fan_session_token.clone().ok_or("active fan session")?,
            idempotency_key: IdempotencyKey::parse("event-interest-0001")?,
            request_id: RequestId::parse("event-interest-request-0001")?,
            campaign_id: None,
            visitor_id: Some(VisitorId::new()),
            source: "integration_test".to_owned(),
        },
    )?;
    let first = events.register_interest(&command).await?;
    assert!(first.created);
    assert_eq!(first.reminder_count, 2);

    let replay = RegisterEventInterestCommand::new(
        crowdrelay_application::RegisterEventInterestCommandArgs {
            workspace_id,
            event_slug: EventSlug::parse("wroclaw-live-2026")?,
            fan_session: fan.fan_session_token.clone().ok_or("active fan session")?,
            idempotency_key: IdempotencyKey::parse("event-interest-0001")?,
            request_id: RequestId::parse("event-interest-request-0001-retry")?,
            campaign_id: None,
            visitor_id: command.visitor_id(),
            source: "integration_test".to_owned(),
        },
    )?;
    assert_eq!(events.register_interest(&replay).await?, first);

    let interests = events
        .list_fan_interests(
            workspace_id,
            fan.fan_session_token.as_ref().ok_or("active fan session")?,
            10,
        )
        .await?;
    assert_eq!(interests.len(), 1);
    assert_eq!(interests[0].event.id.into_uuid(), event_id);

    let action = EventAction::new(
        workspace_id,
        published[0].id,
        EventActionKind::TicketClick,
        None,
        Some(VisitorId::new()),
        Some("virya.music".to_owned()),
        OffsetDateTime::now_utc(),
    )?;
    events.persist_event_action(&[action]).await?;
    let valid_batched_action = EventAction::new(
        workspace_id,
        published[0].id,
        EventActionKind::ListenClick,
        None,
        Some(VisitorId::new()),
        None,
        OffsetDateTime::now_utc(),
    )?;
    let stale_batched_action = EventAction::new(
        workspace_id,
        EventId::new(),
        EventActionKind::ShareClick,
        None,
        Some(VisitorId::new()),
        None,
        OffsetDateTime::now_utc(),
    )?;
    assert_eq!(
        events
            .persist_event_action(&[valid_batched_action, stale_batched_action])
            .await,
        Err(crowdrelay_application::RepositoryError::Conflict)
    );

    let interest_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM event_interests WHERE workspace_id = $1",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(interest_count, 1);

    let reminder_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM event_reminder_jobs WHERE workspace_id = $1",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(reminder_count, 2);

    let action_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM event_action_events WHERE workspace_id = $1",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(action_count, 1);

    let outbox_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM outbox_events WHERE workspace_id = $1 AND event_type = 'event.interest_registered'",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(outbox_count, 1);

    pool.close().await;
    Ok(())
}

fn signup_command(
    workspace_id: WorkspaceId,
) -> Result<SignupFanCommand, Box<dyn std::error::Error>> {
    let signup = FanSignup::new(FanSignupInput {
        workspace_id,
        email: NormalizedEmail::parse("event-fan@example.test")?,
        display_name: Some("Event Fan".to_owned()),
        city_slug: CitySlug::parse("wroclaw")?,
        locale: Some("pl-PL".to_owned()),
        campaign_id: None,
        visitor_id: None,
        claimed_referral_code: None,
        consent: MarketingConsent::new(true, "privacy-2026-07", "integration_test")?,
    })?;
    Ok(SignupFanCommand::new(
        IdempotencyKey::parse("event-fan-signup-0001")?,
        RequestId::parse("event-fan-signup-request-0001")?,
        signup,
    ))
}

async fn seed_fixture(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    slug: &WorkspaceSlug,
    starts_at: OffsetDateTime,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let candidate_city_id = Uuid::now_v7();
    let event_id = Uuid::now_v7();
    let mut transaction = pool.begin().await?;
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(slug.as_str())
        .bind("Events E2E")
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        r#"
        INSERT INTO cities (id, slug, name, country_code)
        VALUES ($1, 'wroclaw', 'Wrocław', 'PL')
        ON CONFLICT (country_code, slug) DO UPDATE SET name = EXCLUDED.name
        "#,
    )
    .bind(candidate_city_id)
    .execute(&mut *transaction)
    .await?;
    let city_id = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM cities WHERE country_code = 'PL' AND slug = 'wroclaw'",
    )
    .fetch_one(&mut *transaction)
    .await?;
    sqlx::query("INSERT INTO city_aggregates (workspace_id, city_id) VALUES ($1, $2)")
        .bind(workspace_id.into_uuid())
        .bind(city_id)
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        r#"
        INSERT INTO events (
            id, workspace_id, city_id, slug, title, description, venue,
            venue_address, timezone, starts_at, doors_at, ends_at, ticket_url,
            listen_url, status, published_at
        ) VALUES (
            $1, $2, $3, 'wroclaw-live-2026', 'Virya live', 'Integration event',
            'Test Club', 'Main Street 1', 'Europe/Warsaw', $4, $5, $6,
            'https://tickets.example.test/virya', 'https://virya.music/music',
            'published', now()
        )
        "#,
    )
    .bind(event_id)
    .bind(workspace_id.into_uuid())
    .bind(city_id)
    .bind(starts_at)
    .bind(starts_at - time::Duration::hours(1))
    .bind(starts_at + time::Duration::hours(3))
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(event_id)
}

fn test_sensitive_response_codec() -> SensitiveResponseCodec {
    SensitiveResponseCodec::new(SensitiveResponseKey::derive_from_secret(
        b"events-integration-response-secret",
    ))
}
