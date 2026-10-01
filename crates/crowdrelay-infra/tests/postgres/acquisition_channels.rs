//! Which channels produced people who stayed — and how many they lost.
//!
//! `signups` nets churn out by construction: a fan who left is not in it.
//! That is the right answer to "how many fans did this channel produce" and
//! no answer at all to "how many did it burn to produce them", which is what
//! `departed` is for.

use std::time::Duration;

use crate::common;
use crowdrelay_application::autopilot::AutopilotControlRepository;
use crowdrelay_domain::WorkspaceId;
use crowdrelay_domain::acquisition_channel::ChannelAttribution;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use time::macros::datetime;
use uuid::Uuid;

async fn repository()
-> Result<(PostgresAutopilotRepository, sqlx::PgPool), Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let database = DatabaseConfig {
        url: database_url,
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

/// One fan who arrived by clicking `link` before signing up, in `status`.
async fn arrive(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    link_id: Uuid,
    label: &str,
    status: &str,
    merged_into: Option<Uuid>,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let ws = workspace_id.into_uuid();
    let visitor = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO click_events (workspace_id, smart_link_id, anonymous_visitor_id, occurred_at)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(ws)
    .bind(link_id)
    .bind(visitor)
    .bind(datetime!(2026-09-01 12:00 UTC))
    .execute(pool)
    .await?;
    let fan_id: Uuid = sqlx::query_scalar(
        "INSERT INTO fans (workspace_id, normalized_email, status, merged_into_fan_id, merged_at)
         VALUES ($1, $2, $3, $4, CASE WHEN $4::uuid IS NULL THEN NULL ELSE now() END)
         RETURNING id",
    )
    .bind(ws)
    .bind(format!("{label}-{}@example.test", ws.simple()))
    .bind(status)
    .bind(merged_into)
    .fetch_one(pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_acquisition_events
             (workspace_id, fan_id, source, request_id, anonymous_visitor_id, occurred_at)
         VALUES ($1, $2, 'public_signup', $3, $4, $5)",
    )
    .bind(ws)
    .bind(fan_id)
    .bind(format!("signup-{label}-{}", ws.simple()))
    .bind(visitor)
    .bind(datetime!(2026-09-01 13:00 UTC))
    .execute(pool)
    .await?;
    Ok(fan_id)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_channel_reports_who_it_lost_beside_who_it_kept() -> Result<(), Box<dyn std::error::Error>>
{
    let (repository, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    let ws = workspace_id.into_uuid();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Channels')")
        .bind(ws)
        .bind(format!("channels-{}", ws.simple()))
        .execute(&pool)
        .await?;
    let link_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO smart_links (id, workspace_id, slug, destination_url, channel_source)
         VALUES ($1, $2, $3, 'https://band.example/signal', 'instagram')",
    )
    .bind(link_id)
    .bind(ws)
    .bind(format!("ig-{}", link_id.simple()))
    .execute(&pool)
    .await?;

    // Two stayed, two left — one unsubscribed, one suppressed (a bounce or a
    // deleted account; either way not a fan we can reach).
    let kept = arrive(&pool, workspace_id, link_id, "kept", "active", None).await?;
    arrive(&pool, workspace_id, link_id, "also-kept", "active", None).await?;
    arrive(
        &pool,
        workspace_id,
        link_id,
        "unsubscribed",
        "unsubscribed",
        None,
    )
    .await?;
    arrive(
        &pool,
        workspace_id,
        link_id,
        "suppressed",
        "suppressed",
        None,
    )
    .await?;
    // Neither of these is a departure. A merged identity is already counted
    // as the fan it merged into; a pending signup never became a fan.
    arrive(&pool, workspace_id, link_id, "merged", "merged", Some(kept)).await?;
    arrive(&pool, workspace_id, link_id, "pending", "pending", None).await?;

    let readout = repository
        .load_acquisition_channels(workspace_id, datetime!(2026-09-20 12:00 UTC))
        .await?;
    assert_eq!(readout.channels.len(), 1, "{readout:?}");
    let channel = &readout.channels[0];
    assert_eq!(channel.signups, 2, "signups keeps its meaning: still here");
    assert_eq!(
        channel.departed, 2,
        "the two who left are counted, not dropped"
    );
    assert_eq!(
        readout.total_signups, 2,
        "totals are unchanged by departures"
    );
    assert!(readout.unattributed.is_empty(), "{readout:?}");
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_channel_everyone_left_still_appears() -> Result<(), Box<dyn std::error::Error>> {
    // Before `departed`, a channel whose arrivals all left vanished from the
    // readout entirely — indistinguishable from a channel nobody ever used.
    let (repository, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    let ws = workspace_id.into_uuid();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Channels')")
        .bind(ws)
        .bind(format!("channels-gone-{}", ws.simple()))
        .execute(&pool)
        .await?;
    let link_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO smart_links (id, workspace_id, slug, destination_url, channel_source)
         VALUES ($1, $2, $3, 'https://band.example/signal', 'facebook')",
    )
    .bind(link_id)
    .bind(ws)
    .bind(format!("fb-{}", link_id.simple()))
    .execute(&pool)
    .await?;
    for label in ["a", "b", "c"] {
        arrive(&pool, workspace_id, link_id, label, "unsubscribed", None).await?;
    }

    let readout = repository
        .load_acquisition_channels(workspace_id, datetime!(2026-09-20 12:00 UTC))
        .await?;
    assert_eq!(readout.channels.len(), 1, "{readout:?}");
    let channel = &readout.channels[0];
    assert_eq!(channel.signups, 0);
    assert_eq!(channel.departed, 3);
    assert_eq!(
        channel.activation_basis_points, None,
        "no one stayed, so there is no rate — not a zero"
    );
    assert!(
        !channel.sufficient_evidence,
        "three produced fans are luck, not evidence"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_floor_separates_insufficient_evidence_from_a_stated_zero()
-> Result<(), Box<dyn std::error::Error>> {
    let (repository, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    let ws = workspace_id.into_uuid();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Floor')")
        .bind(ws)
        .bind(format!("floor-{}", ws.simple()))
        .execute(&pool)
        .await?;

    // Three arrivals under the small link — two stayed, one left. Nobody can
    // read a percentage off three people, so the channel reports evidence
    // below the floor and the rate is suppressed rather than rounded.
    let thin_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO smart_links (id, workspace_id, slug, destination_url, channel_source)
         VALUES ($1, $2, $3, 'https://band.example/signal', 'reddit')",
    )
    .bind(thin_id)
    .bind(ws)
    .bind(format!("thin-{}", thin_id.simple()))
    .execute(&pool)
    .await?;
    arrive(&pool, workspace_id, thin_id, "thin-a", "active", None).await?;
    arrive(&pool, workspace_id, thin_id, "thin-b", "active", None).await?;
    arrive(
        &pool,
        workspace_id,
        thin_id,
        "thin-gone",
        "unsubscribed",
        None,
    )
    .await?;

    // Four arrivals under the fat link — all stayed, none did anything
    // meaningful yet. Four is the floor: the channel can state a real zero
    // because a stated zero now rests on evidence.
    let fat_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO smart_links (id, workspace_id, slug, destination_url, channel_source)
         VALUES ($1, $2, $3, 'https://band.example/signal', 'discord')",
    )
    .bind(fat_id)
    .bind(ws)
    .bind(format!("fat-{}", fat_id.simple()))
    .execute(&pool)
    .await?;
    for label in ["fat-a", "fat-b", "fat-c", "fat-d"] {
        arrive(&pool, workspace_id, fat_id, label, "active", None).await?;
    }

    let readout = repository
        .load_acquisition_channels(workspace_id, datetime!(2026-09-20 12:00 UTC))
        .await?;
    assert_eq!(readout.channels.len(), 2, "{readout:?}");
    let channel = |source: &str| {
        readout
            .channels
            .iter()
            .find(|c| {
                matches!(
                    &c.attribution,
                    ChannelAttribution::Attributed(id) if id.source == source
                )
            })
            .expect(source)
    };

    let thin = channel("reddit");
    assert_eq!(thin.signups, 2);
    assert_eq!(thin.departed, 1);
    assert!(
        !thin.sufficient_evidence,
        "three produced fans are below the floor"
    );
    assert_eq!(
        thin.activation_basis_points, None,
        "the rate is suppressed under the floor — not stated as a number"
    );

    let fat = channel("discord");
    assert_eq!(fat.signups, 4);
    assert_eq!(fat.departed, 0);
    assert!(
        fat.sufficient_evidence,
        "four produced fans clear the floor"
    );
    assert_eq!(
        fat.activation_basis_points,
        Some(0),
        "a stated zero is a claim made on evidence, not a missing number"
    );
    Ok(())
}
