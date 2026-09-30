use std::time::Duration;

use crate::common;
use crowdrelay_application::{
    AcquisitionRepository, EventActEntry, EventRepository, IdempotencyKey,
    RegisterEventInterestCommand, ReplaceEventActsCommand, RequestId, SetEventFestivalCommand,
    SignupFanCommand,
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
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_EVENT_TEST_DATABASE_URL").await?;
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

    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_EVENT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn replaces_event_bill_and_attributes_ticket_clicks_per_act()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_EVENT_TEST_DATABASE_URL").await?;
    let workspace_id = WorkspaceId::new();
    let workspace_slug =
        WorkspaceSlug::parse(format!("event-acts-{}", workspace_id.into_uuid().simple()))?;
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
    let events =
        PostgresEventRepository::new(pool.clone(), workspace_slug, &database, vec![1_440, 120]);

    // Write the bill: two acts, headliner second in running order.
    events
        .replace_event_acts(&ReplaceEventActsCommand {
            workspace_id,
            event_slug: "wroclaw-live-2026".to_owned(),
            acts: vec![
                EventActEntry {
                    act_slug: "virya".to_owned(),
                    act_name: "Virya".to_owned(),
                    position: 1,
                    ticket_url: Some("https://tickets.example.test/virya".to_owned()),
                },
                EventActEntry {
                    act_slug: "opener".to_owned(),
                    act_name: "Opener".to_owned(),
                    position: 0,
                    ticket_url: None,
                },
            ],
        })
        .await?;

    // The bill surfaces on the published event in bill order.
    let published = events.load_published_events().await?;
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].acts.len(), 2);
    assert_eq!(published[0].acts[0].act_slug, "opener");
    assert_eq!(published[0].acts[0].ticket_url, None);
    assert_eq!(published[0].acts[1].act_slug, "virya");
    assert_eq!(
        published[0].acts[1].ticket_url.as_deref(),
        Some("https://tickets.example.test/virya")
    );

    // Replace semantics: a second write swaps the bill atomically.
    events
        .replace_event_acts(&ReplaceEventActsCommand {
            workspace_id,
            event_slug: "wroclaw-live-2026".to_owned(),
            acts: vec![EventActEntry {
                act_slug: "virya".to_owned(),
                act_name: "Virya".to_owned(),
                position: 0,
                ticket_url: Some("https://tickets.example.test/virya".to_owned()),
            }],
        })
        .await?;
    let published = events.load_published_events().await?;
    assert_eq!(published[0].acts.len(), 1);
    assert_eq!(published[0].acts[0].act_slug, "virya");

    // A click attributed to the act lands on the ledger with the slug — the
    // fact survives the act row changing later because it is denormalized.
    let mut action = EventAction::new(
        workspace_id,
        EventId::from_uuid(event_id),
        EventActionKind::TicketClick,
        None,
        Some(VisitorId::new()),
        None,
        OffsetDateTime::now_utc(),
    )?;
    action.set_act_slug("virya")?;
    events.persist_event_action(&[action]).await?;
    let recorded: Option<String> = sqlx::query_scalar(
        "SELECT act_slug FROM event_action_events WHERE workspace_id = $1 AND event_id = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind(event_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(recorded.as_deref(), Some("virya"));

    // An unattributed click stays NULL — not guessed at after the fact.
    let plain = EventAction::new(
        workspace_id,
        EventId::from_uuid(event_id),
        EventActionKind::TicketClick,
        None,
        None,
        None,
        OffsetDateTime::now_utc(),
    )?;
    events.persist_event_action(&[plain]).await?;
    let null_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM event_action_events WHERE workspace_id = $1 AND act_slug IS NULL",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(null_count, 1);

    // Rejections: unknown event, malformed act, duplicate slugs, http URL.
    assert_eq!(
        events
            .replace_event_acts(&ReplaceEventActsCommand {
                workspace_id,
                event_slug: "no-such-show".to_owned(),
                acts: Vec::new(),
            })
            .await,
        Err(crowdrelay_application::RepositoryError::NotFound)
    );
    assert_eq!(
        events
            .replace_event_acts(&ReplaceEventActsCommand {
                workspace_id,
                event_slug: "wroclaw-live-2026".to_owned(),
                acts: vec![EventActEntry {
                    act_slug: "Bad Slug!".to_owned(),
                    act_name: "Bad".to_owned(),
                    position: 0,
                    ticket_url: None,
                }],
            })
            .await,
        Err(crowdrelay_application::RepositoryError::Conflict)
    );
    assert_eq!(
        events
            .replace_event_acts(&ReplaceEventActsCommand {
                workspace_id,
                event_slug: "wroclaw-live-2026".to_owned(),
                acts: vec![
                    EventActEntry {
                        act_slug: "dup".to_owned(),
                        act_name: "One".to_owned(),
                        position: 0,
                        ticket_url: None,
                    },
                    EventActEntry {
                        act_slug: "dup".to_owned(),
                        act_name: "Two".to_owned(),
                        position: 1,
                        ticket_url: None,
                    },
                ],
            })
            .await,
        Err(crowdrelay_application::RepositoryError::Conflict)
    );
    assert_eq!(
        events
            .replace_event_acts(&ReplaceEventActsCommand {
                workspace_id,
                event_slug: "wroclaw-live-2026".to_owned(),
                acts: vec![EventActEntry {
                    act_slug: "virya".to_owned(),
                    act_name: "Virya".to_owned(),
                    position: 0,
                    ticket_url: Some("http://insecure.example.test/x".to_owned()),
                }],
            })
            .await,
        Err(crowdrelay_application::RepositoryError::Conflict)
    );

    Ok(())
}

/// 6.1: a festival slot is an ordinary event carrying `festival_name`. The
/// marker writes through the staff/admin setter, surfaces on the published
/// event read the T-21→T+7 chain consumes, and clears back to an ordinary
/// night. A stranger's workspace cannot touch it.
#[tokio::test]
#[ignore = "requires CROWDRELAY_EVENT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_festival_slot_runs_as_an_ordinary_show() -> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_EVENT_TEST_DATABASE_URL").await?;
    let workspace_id = WorkspaceId::new();
    let workspace_slug =
        WorkspaceSlug::parse(format!("event-fest-{}", workspace_id.into_uuid().simple()))?;
    let starts_at = OffsetDateTime::now_utc() + time::Duration::days(2);
    seed_fixture(&pool, workspace_id, &workspace_slug, starts_at).await?;

    let database = DatabaseConfig {
        url: database_url,
        max_connections: 8,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(1),
    };
    let events =
        PostgresEventRepository::new(pool.clone(), workspace_slug, &database, vec![1_440, 120]);

    // Unmarked: an ordinary night.
    let published = events.load_published_events().await?;
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].festival_name, None);

    // The slot's festival identity lands on the same published read the
    // production chain consumes — no branch, no separate workflow.
    events
        .set_event_festival(&SetEventFestivalCommand {
            workspace_id,
            event_slug: "wroclaw-live-2026".to_owned(),
            festival_name: Some("OFF Festival Katowice".to_owned()),
        })
        .await?;
    let published = events.load_published_events().await?;
    assert_eq!(published.len(), 1);
    assert_eq!(
        published[0].festival_name.as_deref(),
        Some("OFF Festival Katowice")
    );

    // Clearing returns the night to ordinary.
    events
        .set_event_festival(&SetEventFestivalCommand {
            workspace_id,
            event_slug: "wroclaw-live-2026".to_owned(),
            festival_name: None,
        })
        .await?;
    let published = events.load_published_events().await?;
    assert_eq!(published[0].festival_name, None);

    // Another workspace's id cannot reach the row at all.
    assert_eq!(
        events
            .set_event_festival(&SetEventFestivalCommand {
                workspace_id: WorkspaceId::new(),
                event_slug: "wroclaw-live-2026".to_owned(),
                festival_name: Some("Stolen Festival".to_owned()),
            })
            .await,
        Err(crowdrelay_application::RepositoryError::NotFound)
    );

    Ok(())
}

/// 6.2: a festival slot's bill runs longer than a club night's — the same
/// write path, a bound that follows the stored festival mark. An unmarked
/// event refuses a festival-scale bill, a marked one stores it and the
/// published read carries every act; the mark cannot come off while the
/// bill still exceeds the club bound, and no bill passes the absolute cap.
#[tokio::test]
#[ignore = "requires CROWDRELAY_EVENT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_festival_bill_runs_festival_scale() -> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_EVENT_TEST_DATABASE_URL").await?;
    let workspace_id = WorkspaceId::new();
    let workspace_slug =
        WorkspaceSlug::parse(format!("event-fest-{}", workspace_id.into_uuid().simple()))?;
    let starts_at = OffsetDateTime::now_utc() + time::Duration::days(2);
    seed_fixture(&pool, workspace_id, &workspace_slug, starts_at).await?;

    let database = DatabaseConfig {
        url: database_url,
        max_connections: 8,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(1),
    };
    let events =
        PostgresEventRepository::new(pool.clone(), workspace_slug, &database, vec![1_440, 120]);

    let bill = |count: usize| ReplaceEventActsCommand {
        workspace_id,
        event_slug: "wroclaw-live-2026".to_owned(),
        acts: (0..count)
            .map(|index| EventActEntry {
                act_slug: format!("act-{index}"),
                act_name: format!("Act {index}"),
                position: i32::try_from(index).unwrap_or_default(),
                ticket_url: None,
            })
            .collect(),
    };

    // An ordinary night's bill stays club-sized: forty acts refuse.
    assert_eq!(
        events.replace_event_acts(&bill(40)).await,
        Err(crowdrelay_application::RepositoryError::Conflict)
    );

    // The festival mark lifts the same event's bound — forty land whole.
    events
        .set_event_festival(&SetEventFestivalCommand {
            workspace_id,
            event_slug: "wroclaw-live-2026".to_owned(),
            festival_name: Some("OFF Festival Katowice".to_owned()),
        })
        .await?;
    events.replace_event_acts(&bill(40)).await?;
    let published = events.load_published_events().await?;
    assert_eq!(published[0].acts.len(), 40);
    assert_eq!(published[0].acts[0].act_slug, "act-0");
    assert_eq!(published[0].acts[39].act_slug, "act-39");

    // The mark cannot come off while the bill still exceeds the club bound —
    // clearing it would strand acts the public validator then refuses.
    assert_eq!(
        events
            .set_event_festival(&SetEventFestivalCommand {
                workspace_id,
                event_slug: "wroclaw-live-2026".to_owned(),
                festival_name: None,
            })
            .await,
        Err(crowdrelay_application::RepositoryError::Conflict)
    );

    // Past the absolute bound even a festival refuses.
    assert_eq!(
        events
            .replace_event_acts(&bill(crowdrelay_domain::MAX_EVENT_ACTS_FESTIVAL + 1))
            .await,
        Err(crowdrelay_application::RepositoryError::Conflict)
    );

    // Shrinking the bill back to club size lets the mark clear.
    events.replace_event_acts(&bill(32)).await?;
    events
        .set_event_festival(&SetEventFestivalCommand {
            workspace_id,
            event_slug: "wroclaw-live-2026".to_owned(),
            festival_name: None,
        })
        .await?;
    let published = events.load_published_events().await?;
    assert_eq!(published[0].festival_name, None);
    assert_eq!(published[0].acts.len(), 32);

    Ok(())
}

/// 6.5 — the platform knows how many tenant acts shared each bill. The
/// trigger-maintained `tenant_act_count` on events counts bill rows whose
/// act resolved to a platform workspace at bill-write time — the event's own
/// act, a roster sibling, or another tenant — and ignores peer identities
/// and bare names. It climbs when a second tenant lands and falls when the
/// bill shrinks, so the organiser product's density predicate is a WHERE
/// clause, not a nightly scan.
#[tokio::test]
#[ignore = "requires CROWDRELAY_EVENT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn tenant_act_count_tracks_resolved_acts_on_the_bill()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_EVENT_TEST_DATABASE_URL").await?;
    let workspace_id = WorkspaceId::new();
    let workspace_slug =
        WorkspaceSlug::parse(format!("event-acts-{}", workspace_id.into_uuid().simple()))?;
    let starts_at = OffsetDateTime::now_utc() + time::Duration::days(2);
    let event_id = seed_fixture(&pool, workspace_id, &workspace_slug, starts_at).await?;

    // A second tenant act — its workspace slug is what a bill-mate resolves
    // on. The slug is unique per run because the shared test database keeps
    // the rows a previous run wrote.
    let mate_id = WorkspaceId::new();
    let mate_slug = format!("mate-band-{}", mate_id.into_uuid().simple());
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Mate Band')")
        .bind(mate_id.into_uuid())
        .bind(&mate_slug)
        .execute(&pool)
        .await?;

    let database = DatabaseConfig {
        url: database_url,
        max_connections: 8,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(1),
    };
    let events = PostgresEventRepository::new(
        pool.clone(),
        workspace_slug.clone(),
        &database,
        vec![1_440, 120],
    );

    let count = |workspace: WorkspaceId, event: Uuid, pool: &PgPool| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i32>(
                "SELECT tenant_act_count FROM events WHERE workspace_id = $1 AND id = $2",
            )
            .bind(workspace.into_uuid())
            .bind(event)
            .fetch_one(&pool)
            .await
        }
    };

    // A three-act bill: the tenant's own (slug hit), the mate (slug hit on
    // the second workspace), and an outside act that resolves to a peer —
    // which is not a tenant and must not count.
    events
        .replace_event_acts(&ReplaceEventActsCommand {
            workspace_id,
            event_slug: "wroclaw-live-2026".to_owned(),
            acts: vec![
                EventActEntry {
                    act_slug: workspace_slug.as_str().to_owned(),
                    // Unique per run: the shared test database may hold a
                    // band listing naming a common act, and a slug hit plus
                    // a foreign listing hit is ambiguous by design.
                    act_name: format!("Own Act {}", workspace_id.into_uuid().simple()),
                    position: 0,
                    ticket_url: None,
                },
                EventActEntry {
                    act_slug: mate_slug.clone(),
                    act_name: format!("Mate Band {}", mate_id.into_uuid().simple()),
                    position: 1,
                    ticket_url: None,
                },
                EventActEntry {
                    act_slug: "outlander".to_owned(),
                    act_name: format!("Outlander {}", event_id.simple()),
                    position: 2,
                    ticket_url: None,
                },
            ],
        })
        .await?;
    assert_eq!(
        count(workspace_id, event_id, &pool).await?,
        2,
        "two platform acts share this bill; the peer is not one"
    );
    let (ws, peer): (Option<Uuid>, Option<Uuid>) = sqlx::query_as(
        "SELECT act_workspace_id, peer_act_id FROM event_acts
         WHERE workspace_id = $1 AND event_id = $2 AND act_slug = 'outlander'",
    )
    .bind(workspace_id.into_uuid())
    .bind(event_id)
    .fetch_one(&pool)
    .await?;
    assert!(
        ws.is_none() && peer.is_some(),
        "the outside act is a peer, not a tenant"
    );

    // The bill shrinks and the counter follows — density is a fact about the
    // current bill, not a cumulative tally.
    events
        .replace_event_acts(&ReplaceEventActsCommand {
            workspace_id,
            event_slug: "wroclaw-live-2026".to_owned(),
            acts: vec![
                EventActEntry {
                    act_slug: workspace_slug.as_str().to_owned(),
                    act_name: format!("Own Act {}", workspace_id.into_uuid().simple()),
                    position: 0,
                    ticket_url: None,
                },
                EventActEntry {
                    act_slug: "outlander".to_owned(),
                    act_name: format!("Outlander {}", event_id.simple()),
                    position: 1,
                    ticket_url: None,
                },
            ],
        })
        .await?;
    assert_eq!(
        count(workspace_id, event_id, &pool).await?,
        1,
        "the mate leaving drops the count"
    );

    // No platform act on the bill at all is an honest zero.
    events
        .replace_event_acts(&ReplaceEventActsCommand {
            workspace_id,
            event_slug: "wroclaw-live-2026".to_owned(),
            acts: vec![EventActEntry {
                act_slug: "outlander".to_owned(),
                act_name: format!("Outlander {}", event_id.simple()),
                position: 0,
                ticket_url: None,
            }],
        })
        .await?;
    assert_eq!(count(workspace_id, event_id, &pool).await?, 0);

    Ok(())
}

fn signup_command(
    workspace_id: WorkspaceId,
) -> Result<SignupFanCommand, Box<dyn std::error::Error>> {
    let signup = FanSignup::new(FanSignupInput {
        workspace_id,
        email: NormalizedEmail::parse("event-fan@example.test")?,
        display_name: Some("Event Fan".to_owned()),
        city_slug: Some(CitySlug::parse("wroclaw")?),
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

/// §4f-2 venue registry: a published event marks its room, a rename
/// re-points the mark onto the same normalised venue, a cancellation
/// retracts it, and two workspaces naming the same room share one venue
/// row while keeping separate marks — contribution without exposure.
#[tokio::test]
#[ignore = "requires CROWDRELAY_EVENT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn events_mark_the_shared_venue_registry() -> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_EVENT_TEST_DATABASE_URL").await?;
    let workspace_a = WorkspaceId::new();
    let workspace_b = WorkspaceId::new();
    let slug_a = WorkspaceSlug::parse(format!("venue-a-{}", workspace_a.into_uuid().simple()))?;
    let slug_b = WorkspaceSlug::parse(format!("venue-b-{}", workspace_b.into_uuid().simple()))?;
    let starts_at = OffsetDateTime::now_utc() - time::Duration::days(30);
    let event_a = seed_fixture(&pool, workspace_a, &slug_a, starts_at).await?;
    let event_b = seed_fixture(&pool, workspace_b, &slug_b, starts_at).await?;

    // Both fixtures seed venue 'Test Club' in 'wroclaw' → one shared room,
    // two private marks.
    let (venue_count, mark_count, contributors) = sqlx::query_as::<_, (i64, i64, i64)>(
        r#"
        SELECT count(DISTINCT v.id), count(m.id), count(DISTINCT m.workspace_id)
        FROM place_venues v
        JOIN place_venue_marks m ON m.venue_id = v.id
        WHERE m.event_id IN ($1, $2)
        "#,
    )
    .bind(event_a)
    .bind(event_b)
    .fetch_one(&pool)
    .await?;
    assert_eq!(venue_count, 1, "same room in the same city is one venue");
    assert_eq!(mark_count, 2);
    assert_eq!(contributors, 2);

    // Rename with different casing/whitespace → the mark re-points, not
    // duplicates.
    sqlx::query("UPDATE events SET venue = '  test CLUB ' WHERE id = $1")
        .bind(event_a)
        .execute(&pool)
        .await?;
    let name_keys = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*) FROM place_venues v
        JOIN cities c ON c.id = v.city_id
        WHERE c.slug = 'wroclaw' AND v.name_key = 'test club'
        "#,
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(name_keys, 1, "casing/whitespace still one room");
    let still_marked =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM place_venue_marks WHERE event_id = $1")
            .bind(event_a)
            .fetch_one(&pool)
            .await?;
    assert_eq!(still_marked, 1);

    // Cancel → the claim the event made retracts.
    sqlx::query("UPDATE events SET status = 'cancelled' WHERE id = $1")
        .bind(event_a)
        .execute(&pool)
        .await?;
    let after_cancel =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM place_venue_marks WHERE event_id = $1")
            .bind(event_a)
            .fetch_one(&pool)
            .await?;
    assert_eq!(after_cancel, 0, "a cancelled show never happened");

    // Draft inserts mark nothing.
    let draft_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO events (
            id, workspace_id, city_id, slug, title, venue, starts_at, status
        )
        SELECT $1, workspace_id, city_id, 'draft-no-mark', 'Draft', 'Test Club',
               now() + interval '30 days', 'draft'
        FROM events WHERE id = $2
        "#,
    )
    .bind(draft_id)
    .bind(event_b)
    .execute(&pool)
    .await?;
    let draft_marks =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM place_venue_marks WHERE event_id = $1")
            .bind(draft_id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(draft_marks, 0, "a draft is not a played room");

    // Losing the venue name retracts the mark the same way cancel does.
    sqlx::query("UPDATE events SET venue = NULL WHERE id = $1")
        .bind(event_b)
        .execute(&pool)
        .await?;
    let after_null =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM place_venue_marks WHERE event_id = $1")
            .bind(event_b)
            .fetch_one(&pool)
            .await?;
    assert_eq!(after_null, 0, "no named room, no claim");

    // A name past the domain's bound does not mark — and must not fail the
    // event write that triggered it.
    let oversized = "x".repeat(501);
    sqlx::query("UPDATE events SET venue = $1, status = 'published' WHERE id = $2")
        .bind(&oversized)
        .bind(event_b)
        .execute(&pool)
        .await?;
    let oversized_marks =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM place_venue_marks WHERE event_id = $1")
            .bind(event_b)
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        oversized_marks, 0,
        "an over-length name is not a room record"
    );

    // Deleting the event cascades its mark.
    sqlx::query("UPDATE events SET venue = 'Test Club' WHERE id = $1")
        .bind(event_b)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM events WHERE id = $1")
        .bind(event_b)
        .execute(&pool)
        .await?;
    let after_delete =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM place_venue_marks WHERE event_id = $1")
            .bind(event_b)
            .fetch_one(&pool)
            .await?;
    assert_eq!(after_delete, 0, "delete cascades");
    Ok(())
}

async fn seed_workspace(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    slug: &WorkspaceSlug,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(slug.as_str())
        .bind("Events E2E")
        .execute(pool)
        .await?;
    Ok(())
}

fn create_command(
    workspace_id: WorkspaceId,
    title: &str,
    key: &str,
    request: &str,
) -> Result<crowdrelay_application::CreateEventCommand, Box<dyn std::error::Error>> {
    Ok(crowdrelay_application::CreateEventCommand {
        workspace_id,
        slug_base: crowdrelay_domain::slugify(title).ok_or("a title that folds to a slug")?,
        title: title.to_owned(),
        timezone: None,
        starts_at: OffsetDateTime::now_utc() + time::Duration::days(30),
        doors_at: None,
        ends_at: None,
        venue: None,
        venue_address: None,
        city_name: None,
        city_country_code: None,
        city_region: None,
        ticket_url: None,
        publish: false,
        idempotency_key: IdempotencyKey::parse(key)?,
        request_id: RequestId::parse(request)?,
    })
}

/// The whole point of the manual entry: an operator types the night in, the
/// row lands where every show surface reads, and a retried submit answers
/// the show it already booked instead of double-booking.
#[tokio::test]
#[ignore = "requires CROWDRELAY_EVENT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn manual_show_entry_creates_publishes_and_replays() -> Result<(), Box<dyn std::error::Error>>
{
    let database_url = std::env::var("CROWDRELAY_EVENT_TEST_DATABASE_URL").map_err(|e| {
        format!("CROWDRELAY_EVENT_TEST_DATABASE_URL must target a disposable database: {e}")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&database_url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;

    let workspace_id = WorkspaceId::new();
    let workspace_slug = WorkspaceSlug::parse(format!(
        "event-create-{}",
        workspace_id.into_uuid().simple()
    ))?;
    seed_workspace(&pool, workspace_id, &workspace_slug).await?;
    let database = DatabaseConfig {
        url: database_url.clone(),
        max_connections: 8,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(1),
    };
    let events =
        PostgresEventRepository::new(pool.clone(), workspace_slug, &database, vec![1_440, 120]);

    let starts_at = OffsetDateTime::now_utc() + time::Duration::days(30);
    let command = crowdrelay_application::CreateEventCommand {
        slug_base: crowdrelay_domain::slugify("Virya — Warszawa, Progresja")
            .ok_or("a title that folds")?,
        title: "Virya — Warszawa, Progresja".to_owned(),
        venue: Some("Progresja".to_owned()),
        venue_address: Some("Fort Wola 22".to_owned()),
        city_name: Some("Warszawa".to_owned()),
        city_country_code: Some("PL".to_owned()),
        city_region: Some("mazowieckie".to_owned()),
        ticket_url: Some("https://tickets.example.test/virya-warszawa".to_owned()),
        doors_at: Some(starts_at - time::Duration::hours(1)),
        starts_at,
        ends_at: Some(starts_at + time::Duration::hours(3)),
        publish: true,
        ..create_command(
            workspace_id,
            "unused",
            "event-create-0001",
            "event-create-request-0001",
        )?
    };
    let created = events.create_event(&command).await?;
    assert_eq!(created.slug, "virya-warszawa-progresja");
    assert_eq!(created.status, "published");

    // The stored row carries what the form was asked for — and the timezone
    // default resolved without one.
    let stored = sqlx::query_as::<_, (String, Option<String>, String, bool)>(
        r#"
        SELECT e.status, c.slug, e.timezone, e.published_at IS NOT NULL
        FROM events e
        LEFT JOIN cities c ON c.id = e.city_id
        WHERE e.id = $1
        "#,
    )
    .bind(created.event_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored.0, "published");
    assert_eq!(stored.1.as_deref(), Some("warszawa"));
    assert_eq!(stored.2, "Europe/Warsaw");
    assert!(stored.3, "a published show stamps when it announced");

    // The public read path — the same list the fan site renders — sees it.
    let published = events.load_published_events().await?;
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].id, created.event_id);

    // The replay protocol: same key + same body answers the original, even
    // under a fresh request id; same key + a different body refuses.
    let replayed = events.create_event(&command).await?;
    assert_eq!(replayed, created);
    let retry_new_request = crowdrelay_application::CreateEventCommand {
        request_id: RequestId::parse("event-create-request-0001-retry")?,
        ..command
    };
    assert_eq!(events.create_event(&retry_new_request).await?, created);
    let tampered = crowdrelay_application::CreateEventCommand {
        title: "A different night".to_owned(),
        ..retry_new_request
    };
    assert!(matches!(
        events.create_event(&tampered).await,
        Err(crowdrelay_application::RepositoryError::Conflict)
    ));
    let event_count =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM events WHERE workspace_id = $1")
            .bind(workspace_id.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(event_count, 1, "replays and refusals book nothing twice");
    Ok(())
}

/// `publish: false` is the form's other half — the night exists for every
/// internal read while the public list stays silent until announcing.
#[tokio::test]
#[ignore = "requires CROWDRELAY_EVENT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn manual_show_entry_keeps_a_draft_off_the_public_list()
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
        WorkspaceSlug::parse(format!("event-draft-{}", workspace_id.into_uuid().simple()))?;
    seed_workspace(&pool, workspace_id, &workspace_slug).await?;
    let database = DatabaseConfig {
        url: database_url,
        max_connections: 8,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(1),
    };
    let events =
        PostgresEventRepository::new(pool.clone(), workspace_slug, &database, vec![1_440, 120]);

    let created = events
        .create_event(&create_command(
            workspace_id,
            "Quiet Tuesday",
            "event-draft-0001",
            "event-draft-request-0001",
        )?)
        .await?;
    assert_eq!(created.status, "draft");
    assert!(events.load_published_events().await?.is_empty());
    let stored_status = sqlx::query_scalar::<_, String>("SELECT status FROM events WHERE id = $1")
        .bind(created.event_id.into_uuid())
        .fetch_one(&pool)
        .await?;
    assert_eq!(stored_status, "draft");
    Ok(())
}

/// Two same-titled nights walk the `-N` suffix rather than colliding, and a
/// command naming a foreign workspace never reaches the row — the repository
/// answers NotFound the same way every other write does.
#[tokio::test]
#[ignore = "requires CROWDRELAY_EVENT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn manual_show_entry_walks_slug_collisions_and_stays_in_workspace()
-> Result<(), Box<dyn std::error::Error>> {
    let database_url = std::env::var("CROWDRELAY_EVENT_TEST_DATABASE_URL").map_err(|e| {
        format!("CROWDRELAY_EVENT_TEST_DATABASE_URL must target a disposable database: {e}")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&database_url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;

    let workspace_a = WorkspaceId::new();
    let workspace_b = WorkspaceId::new();
    let slug_a = WorkspaceSlug::parse(format!("event-a-{}", workspace_a.into_uuid().simple()))?;
    let slug_b = WorkspaceSlug::parse(format!("event-b-{}", workspace_b.into_uuid().simple()))?;
    seed_workspace(&pool, workspace_a, &slug_a).await?;
    seed_workspace(&pool, workspace_b, &slug_b).await?;
    let database = DatabaseConfig {
        url: database_url,
        max_connections: 8,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(1),
    };
    let events_a = PostgresEventRepository::new(pool.clone(), slug_a, &database, vec![1_440, 120]);
    let events_b = PostgresEventRepository::new(pool.clone(), slug_b, &database, vec![1_440, 120]);

    let first = events_a
        .create_event(&create_command(
            workspace_a,
            "Virya live",
            "event-a-0001",
            "event-a-request-0001",
        )?)
        .await?;
    let second = events_a
        .create_event(&create_command(
            workspace_a,
            "Virya live",
            "event-a-0002",
            "event-a-request-0002",
        )?)
        .await?;
    assert_eq!(first.slug, "virya-live");
    assert_eq!(second.slug, "virya-live-2");

    // A command carrying workspace B's id at workspace A's repository is a
    // foreign write — NotFound, and no row lands under either workspace.
    assert!(matches!(
        events_a
            .create_event(&create_command(
                workspace_b,
                "Virya live",
                "event-b-foreign",
                "event-b-foreign-request",
            )?)
            .await,
        Err(crowdrelay_application::RepositoryError::NotFound)
    ));
    // Workspace B's own repository takes the same title fresh — slugs are
    // per-workspace.
    let b_first = events_b
        .create_event(&create_command(
            workspace_b,
            "Virya live",
            "event-b-0001",
            "event-b-request-0001",
        )?)
        .await?;
    assert_eq!(b_first.slug, "virya-live");
    let counts = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM events WHERE workspace_id = $1 OR workspace_id = $2",
    )
    .bind(workspace_a.into_uuid())
    .bind(workspace_b.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        counts, 3,
        "two for A, one for B, none for the foreign write"
    );
    Ok(())
}

fn test_sensitive_response_codec() -> SensitiveResponseCodec {
    SensitiveResponseCodec::new(SensitiveResponseKey::derive_from_secret(
        b"events-integration-response-secret",
    ))
}

/// A fan's "my events" list is a live read: a show cancelled after they
/// registered interest must drop out of it — its slug 404s and its ticket
/// link is dead — while a completed one stays, because the show they went
/// to is part of their history. Interest rows are never deleted, so the
/// filter is what keeps the two apart.
#[tokio::test]
#[ignore = "requires CROWDRELAY_EVENT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_cancelled_show_leaves_the_fans_interest_list() -> Result<(), Box<dyn std::error::Error>>
{
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_EVENT_TEST_DATABASE_URL").await?;
    let workspace_id = WorkspaceId::new();
    let workspace_slug = WorkspaceSlug::parse(format!(
        "event-cancel-{}",
        workspace_id.into_uuid().simple()
    ))?;
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
    let command = RegisterEventInterestCommand::new(
        crowdrelay_application::RegisterEventInterestCommandArgs {
            workspace_id,
            event_slug: EventSlug::parse("wroclaw-live-2026")?,
            fan_session: fan.fan_session_token.clone().ok_or("active fan session")?,
            idempotency_key: IdempotencyKey::parse("event-interest-cancel-0001")?,
            request_id: RequestId::parse("event-interest-cancel-request-0001")?,
            campaign_id: None,
            visitor_id: Some(VisitorId::new()),
            source: "integration_test".to_owned(),
        },
    )?;
    assert!(events.register_interest(&command).await?.created);

    let listed = events
        .list_fan_interests(
            workspace_id,
            fan.fan_session_token.as_ref().ok_or("active fan session")?,
            10,
        )
        .await?;
    assert_eq!(listed.len(), 1, "the published show is listed");

    sqlx::query("UPDATE events SET status = 'cancelled' WHERE id = $1")
        .bind(event_id)
        .execute(&pool)
        .await?;
    let listed = events
        .list_fan_interests(
            workspace_id,
            fan.fan_session_token.as_ref().ok_or("active fan session")?,
            10,
        )
        .await?;
    assert!(
        listed.is_empty(),
        "a cancelled show must not surface to the fan — its links are dead"
    );

    sqlx::query("UPDATE events SET status = 'completed' WHERE id = $1")
        .bind(event_id)
        .execute(&pool)
        .await?;
    let listed = events
        .list_fan_interests(
            workspace_id,
            fan.fan_session_token.as_ref().ok_or("active fan session")?,
            10,
        )
        .await?;
    assert_eq!(listed.len(), 1, "a completed show stays in the list");
    Ok(())
}
